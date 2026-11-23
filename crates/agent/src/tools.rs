//! 11-tool agent execution surface, JSON schemas, strict guardrails, and
//! `AgentWorkbookSink` / `HeadlessEvalSink` abstractions (`18-m4.md` §6.2).

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use reticula_core::format::{CurrencySymbol, NumberFormat};
use reticula_core::id::{ColId, IdAllocator, IdRun, RowId, SheetId, pack_sheet_id};
use reticula_core::input::{InferredInput, infer_input_for_cell};
use reticula_core::ops::{
    CellEdit, CellInput, FormatPatch, FormatTarget, FormulaMode, Op, ScalarValue, Workbook,
};
use reticula_core::style::{ColorPatch, ColorValue, HorizontalAlign, StylePatch};
use reticula_core::value::Value;
use reticula_formula::address::parse_cell_addr;
use reticula_formula::binder::{BoundFormula, templatize_formula};
use reticula_formula::engine::{FormulaEngine, TransientEvalValue};
use reticula_formula::search::{FindOptions, SearchIn, SearchScope, find_all};
use reticula_formula::sort::{SortKey, plan_sort};
use reticula_formula::value_eval::EvalScalar;
use reticula_wire::convert_v3::ops_to_wire_txn_v3;
use reticula_wire::v3::WireCommittedTxnV3;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::context::{
    a1_of, build_workbook_overview, parse_a1_range, read_range_compressed, resolve_sheet_id,
};

pub const MAX_WRITE_CELLS_PER_CALL: usize = 2_000;
pub const MAX_STRUCT_ROWS_PER_CALL: u32 = 10_000;
pub const MAX_STRUCT_COLS_PER_CALL: u32 = 256;

/// Boxed future helper for object-safe async traits without adding `async-trait`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Presence highlight emitted as the agent reads or writes a cell/range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPresenceHint {
    pub sheet: SheetId,
    pub row: u32,
    pub col: u32,
    pub end_row: u32,
    pub end_col: u32,
}

/// Abstract workbook target implemented by both `ServerBranchSink` (live draft
/// branch `DocActor`) and `HeadlessEvalSink` (offline eval & replay recording).
pub trait AgentWorkbookSink: Send {
    fn engine(&self) -> &FormulaEngine;
    fn engine_mut(&mut self) -> &mut FormulaEngine;
    fn alloc_row_run(&mut self, len: u32) -> Result<IdRun, String>;
    fn alloc_col_run(&mut self, len: u32) -> Result<IdRun, String>;
    fn alloc_sheet_id(&mut self) -> Result<SheetId, String>;
    fn commit_ops<'a>(
        &'a mut self,
        label: String,
        ops: Vec<Op<BoundFormula>>,
        presence: Option<AgentPresenceHint>,
    ) -> BoxFuture<'a, Result<u64, String>>;
    fn emit_presence<'a>(&'a mut self, presence: AgentPresenceHint) -> BoxFuture<'a, ()>;
}

/// In-memory headless workbook sink used by tests, `replay.rs`, `spike s6`, and
/// `cargo xtask eval`. Tracks touched cell coordinates and whether any
/// structural edit occurred so XLSX export chooses `patch_xlsx` vs `export_xlsx`.
pub struct HeadlessEvalSink {
    engine: FormulaEngine,
    epoch: u8,
    replica: u32,
    user_id: [u8; 16],
    id_alloc: IdAllocator,
    next_sheet_counter: u8,
    next_seq: u64,
    pub committed_txns: Vec<WireCommittedTxnV3>,
    pub touched_cells: BTreeSet<(String, u32, u32)>,
    pub has_structural_ops: bool,
    pub presence_history: Vec<AgentPresenceHint>,
}

impl HeadlessEvalSink {
    pub fn new(workbook: Workbook<BoundFormula>, epoch: u8, replica: u32) -> Result<Self, String> {
        let id_alloc = IdAllocator::new(epoch, replica, 1)
            .ok_or_else(|| "cannot create headless IdAllocator".to_owned())?;
        Ok(Self {
            engine: FormulaEngine::from_workbook_recalc_all(workbook),
            epoch,
            replica,
            user_id: [0xA1; 16],
            id_alloc,
            next_sheet_counter: 1,
            next_seq: 1,
            committed_txns: Vec::new(),
            touched_cells: BTreeSet::new(),
            has_structural_ops: false,
            presence_history: Vec::new(),
        })
    }

    pub fn from_engine(engine: FormulaEngine, epoch: u8, replica: u32) -> Result<Self, String> {
        let id_alloc = IdAllocator::new(epoch, replica, 1)
            .ok_or_else(|| "cannot create headless IdAllocator".to_owned())?;
        Ok(Self {
            engine,
            epoch,
            replica,
            user_id: [0xA1; 16],
            id_alloc,
            next_sheet_counter: 1,
            next_seq: 1,
            committed_txns: Vec::new(),
            touched_cells: BTreeSet::new(),
            has_structural_ops: false,
            presence_history: Vec::new(),
        })
    }

    fn record_touched_for_op(&mut self, op: &Op<BoundFormula>) {
        match op {
            Op::AddSheet { .. }
            | Op::DeleteSheet { .. }
            | Op::RenameSheet { .. }
            | Op::MoveSheet { .. }
            | Op::InsertRows { .. }
            | Op::DeleteRows { .. }
            | Op::InsertCols { .. }
            | Op::DeleteCols { .. }
            | Op::MoveRows { .. }
            | Op::MoveCols { .. } => {
                self.has_structural_ops = true;
            }
            Op::SetCells { sheet, edits } => {
                if let Some(s) = self.engine.workbook().sheet(*sheet) {
                    let sname = s.name().to_owned();
                    for e in edits {
                        if let (Some(rp), Some(cp)) = (
                            s.row_axis().id_to_pos(e.row.0),
                            s.col_axis().id_to_pos(e.col.0),
                        ) {
                            self.touched_cells.insert((sname.clone(), rp, cp));
                        }
                    }
                }
            }
            Op::FillFormula {
                sheet, rows, cols, ..
            } => {
                if let Some(s) = self.engine.workbook().sheet(*sheet) {
                    let sname = s.name().to_owned();
                    for r in rows.iter().flat_map(|run| run.iter()) {
                        for c in cols.iter().flat_map(|run| run.iter()) {
                            if let (Some(rp), Some(cp)) =
                                (s.row_axis().id_to_pos(r), s.col_axis().id_to_pos(c))
                            {
                                self.touched_cells.insert((sname.clone(), rp, cp));
                            }
                        }
                    }
                }
            }
            Op::PermuteContent {
                sheet, rows, cols, ..
            } => {
                if let Some(s) = self.engine.workbook().sheet(*sheet) {
                    let sname = s.name().to_owned();
                    for r in rows {
                        for c in cols {
                            if let (Some(rp), Some(cp)) =
                                (s.row_axis().id_to_pos(r.0), s.col_axis().id_to_pos(c.0))
                            {
                                self.touched_cells.insert((sname.clone(), rp, cp));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl AgentWorkbookSink for HeadlessEvalSink {
    fn engine(&self) -> &FormulaEngine {
        &self.engine
    }

    fn engine_mut(&mut self) -> &mut FormulaEngine {
        &mut self.engine
    }

    fn alloc_row_run(&mut self, len: u32) -> Result<IdRun, String> {
        self.id_alloc
            .alloc_run(len)
            .ok_or_else(|| "row ID space exhausted".to_owned())
    }

    fn alloc_col_run(&mut self, len: u32) -> Result<IdRun, String> {
        self.id_alloc
            .alloc_run(len)
            .ok_or_else(|| "column ID space exhausted".to_owned())
    }

    fn alloc_sheet_id(&mut self) -> Result<SheetId, String> {
        let mut c = self.next_sheet_counter.max(1);
        loop {
            let sid = pack_sheet_id(self.epoch, self.replica, c)
                .ok_or_else(|| "sheet ID space exhausted".to_owned())?;
            if self.engine.workbook().sheet(sid).is_none() {
                self.next_sheet_counter = c.saturating_add(1);
                return Ok(sid);
            }
            if c == u8::MAX {
                return Err("sheet ID space exhausted".to_owned());
            }
            c = c.saturating_add(1);
        }
    }

    fn commit_ops<'a>(
        &'a mut self,
        label: String,
        ops: Vec<Op<BoundFormula>>,
        presence: Option<AgentPresenceHint>,
    ) -> BoxFuture<'a, Result<u64, String>> {
        Box::pin(async move {
            for op in &ops {
                self.engine
                    .apply_op(op.clone())
                    .map_err(|e| format!("op failed: {e:?}"))?;
                self.record_touched_for_op(op);
            }
            self.engine.recalc_all();
            let txn = ops_to_wire_txn_v3(label, &ops)
                .map_err(|e| format!("wire encode failed: {e:?}"))?;
            let seq = self.next_seq;
            self.next_seq += 1;
            self.committed_txns.push(WireCommittedTxnV3 {
                seq,
                replica: self.replica,
                client_seq: seq,
                user_id: self.user_id,
                timestamp_ms: seq * 1000,
                txn,
            });
            if let Some(p) = presence {
                self.presence_history.push(p);
            }
            Ok(seq)
        })
    }

    fn emit_presence<'a>(&'a mut self, presence: AgentPresenceHint) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.presence_history.push(presence);
        })
    }
}

/// Outcome returned by executing a single tool call.
#[derive(Clone, Debug)]
pub struct ToolExecutionOutcome {
    pub output: String,
    pub committed_seq: Option<u64>,
    pub finished: Option<FinishReport>,
    pub presence: Option<AgentPresenceHint>,
}

/// Final structured summary emitted by the `finish` tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishReport {
    pub summary: String,
    #[serde(default)]
    pub highlights: Vec<String>,
}

/// OpenAI-compatible JSON schemas for all 11 spreadsheet analyst tools.
pub fn tool_definitions() -> Vec<JsonValue> {
    vec![
        tool_def(
            "workbook_overview",
            "Inspect workbook worksheets, used ranges, structural anchor rows, column types/stats, named ranges, and error counts.",
            json!({
                "type": "object",
                "properties": {
                    "sheet": { "type": "string", "description": "Optional worksheet name filter" },
                    "max_cols": { "type": "integer", "minimum": 1, "maximum": 64 }
                }
            }),
        ),
        tool_def(
            "read_range",
            "Read formatted values and optional formulas in an A1 range with blank/repeat row compression (max 16 KiB).",
            json!({
                "type": "object",
                "properties": {
                    "sheet": { "type": "string" },
                    "range": { "type": "string", "description": "A1 cell or range such as A1:F30" },
                    "include_formulas": { "type": "boolean" },
                    "max_rows": { "type": "integer", "minimum": 1, "maximum": 500 }
                },
                "required": ["range"]
            }),
        ),
        tool_def(
            "search_cells",
            "Search populated cells by text or regular expression across values or formulas.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "sheet": { "type": "string" },
                    "regex": { "type": "boolean" },
                    "in_formulas": { "type": "boolean" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                },
                "required": ["query"]
            }),
        ),
        tool_def(
            "evaluate_formula",
            "Dry-run a formula at an anchor cell against the live workbook without writing or mutating state.",
            json!({
                "type": "object",
                "properties": {
                    "sheet": { "type": "string" },
                    "cell": { "type": "string", "description": "Anchor A1 coordinate, e.g. D2" },
                    "formula": { "type": "string", "description": "Formula expression, e.g. =SUMIFS(C2:C20,A2:A20,\"East\")" }
                },
                "required": ["cell", "formula"]
            }),
        ),
        tool_def(
            "write_cells",
            "Write scalar values, formulas, and optional number formats to up to 2,000 cells in one atomic transaction on the draft branch.",
            json!({
                "type": "object",
                "properties": {
                    "sheet": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "maxItems": 2000,
                        "items": {
                            "type": "object",
                            "properties": {
                                "cell": { "type": "string" },
                                "value": {},
                                "formula": { "type": "string" },
                                "number_format": { "type": "string" }
                            },
                            "required": ["cell"]
                        }
                    }
                },
                "required": ["edits"]
            }),
        ),
        tool_def(
            "fill_formula",
            "Fill a formula pattern from source_cell across target_range using a compact template op.",
            json!({
                "type": "object",
                "properties": {
                    "sheet": { "type": "string" },
                    "source_cell": { "type": "string" },
                    "formula": { "type": "string" },
                    "target_range": { "type": "string" }
                },
                "required": ["source_cell", "formula", "target_range"]
            }),
        ),
        tool_def(
            "edit_structure",
            "Perform structural workbook mutations (insert_rows, delete_rows, insert_cols, delete_cols, add_sheet, rename_sheet, delete_sheet, move_rows, move_cols, define_named_range, delete_named_range).",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "insert_rows", "delete_rows", "insert_cols", "delete_cols",
                            "add_sheet", "rename_sheet", "delete_sheet",
                            "move_rows", "move_cols", "define_named_range", "delete_named_range"
                        ]
                    },
                    "sheet": { "type": "string" },
                    "index": { "type": "integer", "minimum": 0, "description": "0-based row/col position for insert/delete/move" },
                    "count": { "type": "integer", "minimum": 1 },
                    "target_index": { "type": "integer", "minimum": 0 },
                    "name": { "type": "string" },
                    "formula": { "type": "string" },
                    "workbook_scope": { "type": "boolean" }
                },
                "required": ["action"]
            }),
        ),
        tool_def(
            "sort_range",
            "Sort rows in an A1 rectangle by one or more columns using Excel ordering.",
            json!({
                "type": "object",
                "properties": {
                    "sheet": { "type": "string" },
                    "range": { "type": "string" },
                    "has_header": { "type": "boolean" },
                    "keys": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "column": { "type": "integer", "minimum": 0, "description": "0-based column offset within range" },
                                "descending": { "type": "boolean" }
                            },
                            "required": ["column"]
                        }
                    }
                },
                "required": ["range", "keys"]
            }),
        ),
        tool_def(
            "format_range",
            "Apply number format and/or style patch (bold, italic, align, fill_color, text_color) to an A1 range.",
            json!({
                "type": "object",
                "properties": {
                    "sheet": { "type": "string" },
                    "range": { "type": "string" },
                    "number_format": { "type": "string" },
                    "style": {
                        "type": "object",
                        "properties": {
                            "bold": { "type": "boolean" },
                            "italic": { "type": "boolean" },
                            "align": { "type": "string", "enum": ["left", "center", "right"] },
                            "fill_color": { "type": "string" },
                            "text_color": { "type": "string" }
                        }
                    }
                },
                "required": ["range"]
            }),
        ),
        tool_def(
            "list_errors",
            "List calculation error cells (#DIV/0!, #REF!, #VALUE!, #NAME?, #N/A, #NUM!, #SPILL!, #CYCLE!) and their direct precedents.",
            json!({
                "type": "object",
                "properties": {
                    "sheet": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                }
            }),
        ),
        tool_def(
            "finish",
            "Verify recalculation is clean and finalize the draft with a concise human-readable summary.",
            json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string" },
                    "highlights": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["summary"]
            }),
        ),
    ]
}

fn tool_def(name: &str, description: &str, parameters: JsonValue) -> JsonValue {
    let mut func = serde_json::Map::new();
    func.insert("name".to_owned(), json!(name));
    func.insert("description".to_owned(), json!(description));
    func.insert("parameters".to_owned(), parameters);
    json!({
        "type": "function",
        "function": JsonValue::Object(func),
    })
}

/// Executes one tool invocation against `sink`.
pub async fn execute_tool(
    name: &str,
    args: &JsonValue,
    sink: &mut dyn AgentWorkbookSink,
) -> Result<ToolExecutionOutcome, String> {
    match name {
        "workbook_overview" => {
            let sheet = args.get("sheet").and_then(JsonValue::as_str);
            let max_cols = args
                .get("max_cols")
                .and_then(JsonValue::as_u64)
                .unwrap_or(32) as u32;
            let output = build_workbook_overview(sink.engine(), sheet, max_cols);
            Ok(ToolExecutionOutcome {
                output,
                committed_seq: None,
                finished: None,
                presence: None,
            })
        }
        "read_range" => {
            let sheet = args.get("sheet").and_then(JsonValue::as_str);
            let range = args
                .get("range")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "read_range requires `range`".to_owned())?;
            let include_formulas = args
                .get("include_formulas")
                .and_then(JsonValue::as_bool)
                .unwrap_or(true);
            let max_rows = args
                .get("max_rows")
                .and_then(JsonValue::as_u64)
                .unwrap_or(100) as u32;
            let (sid, (r0, c0, r1, c1), output) =
                read_range_compressed(sink.engine(), sheet, range, include_formulas, max_rows)?;
            let presence = AgentPresenceHint {
                sheet: sid,
                row: r0,
                col: c0,
                end_row: r1,
                end_col: c1,
            };
            sink.emit_presence(presence).await;
            Ok(ToolExecutionOutcome {
                output,
                committed_seq: None,
                finished: None,
                presence: Some(presence),
            })
        }
        "search_cells" => {
            let query = args
                .get("query")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "search_cells requires `query`".to_owned())?;
            let sheet_name = args.get("sheet").and_then(JsonValue::as_str);
            let use_regex = args
                .get("regex")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            let in_formulas = args
                .get("in_formulas")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            let limit = args
                .get("limit")
                .and_then(JsonValue::as_u64)
                .unwrap_or(50)
                .clamp(1, 100) as usize;
            let active_sheet = resolve_sheet_id(sink.engine(), sheet_name)?;
            let opts = FindOptions {
                query: query.to_owned(),
                scope: if sheet_name.is_some() {
                    SearchScope::ActiveSheet
                } else {
                    SearchScope::WholeWorkbook
                },
                search_in: if in_formulas {
                    SearchIn::Formulas
                } else {
                    SearchIn::DisplayValues
                },
                case_sensitive: false,
                whole_cell: false,
                use_regex,
                skip_hidden: false,
            };
            let res = find_all(sink.engine(), active_sheet, &opts, limit)
                .map_err(|e| format!("search failed: {e:?}"))?;
            let items: Vec<JsonValue> = res
                .matches
                .iter()
                .map(|m| {
                    json!({
                        "sheet": m.sheet_name,
                        "cell": a1_of(m.row_pos, m.col_pos),
                        "text": m.text,
                        "isFormula": m.is_formula,
                    })
                })
                .collect();
            Ok(ToolExecutionOutcome {
                output: serde_json::to_string(&json!({
                    "matches": items,
                    "truncated": res.truncated
                }))
                .unwrap_or_default(),
                committed_seq: None,
                finished: None,
                presence: None,
            })
        }
        "evaluate_formula" => {
            let sheet_name = args.get("sheet").and_then(JsonValue::as_str);
            let cell = args
                .get("cell")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "evaluate_formula requires `cell`".to_owned())?;
            let formula = args
                .get("formula")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "evaluate_formula requires `formula`".to_owned())?;
            let sid = resolve_sheet_id(sink.engine(), sheet_name)?;
            let addr = parse_cell_addr(cell.trim())
                .ok_or_else(|| format!("invalid anchor cell `{cell}`"))?;
            let val = sink
                .engine_mut()
                .evaluate_transient_formula(sid, (addr.row, addr.col), formula)
                .map_err(|e| format!("transient evaluation failed: {e:?}"))?;
            let payload = match val {
                TransientEvalValue::Scalar(s) => json!({
                    "kind": "scalar",
                    "value": eval_scalar_to_json(&s),
                }),
                TransientEvalValue::Array(arr) => {
                    let preview: Vec<JsonValue> = arr
                        .values
                        .iter()
                        .take(32)
                        .map(eval_scalar_to_json)
                        .collect();
                    json!({
                        "kind": "array",
                        "rows": arr.rows,
                        "cols": arr.cols,
                        "preview": preview,
                    })
                }
            };
            Ok(ToolExecutionOutcome {
                output: payload.to_string(),
                committed_seq: None,
                finished: None,
                presence: None,
            })
        }
        "write_cells" => exec_write_cells(args, sink).await,
        "fill_formula" => exec_fill_formula(args, sink).await,
        "edit_structure" => exec_edit_structure(args, sink).await,
        "sort_range" => exec_sort_range(args, sink).await,
        "format_range" => exec_format_range(args, sink).await,
        "list_errors" => {
            let sheet_name = args.get("sheet").and_then(JsonValue::as_str);
            let limit = args
                .get("limit")
                .and_then(JsonValue::as_u64)
                .unwrap_or(50)
                .clamp(1, 100) as usize;
            let filter = if sheet_name.is_some() {
                Some(resolve_sheet_id(sink.engine(), sheet_name)?)
            } else {
                None
            };
            let errors = sink.engine().list_error_cells(filter, limit);
            let rows: Vec<JsonValue> = errors
                .iter()
                .map(|e| {
                    json!({
                        "sheet": e.sheet_name,
                        "cell": e.address,
                        "error": e.error,
                        "formula": e.formula,
                        "precedents": e.precedents.iter().map(|p| json!({
                            "sheet": p.sheet_name,
                            "cell": p.address,
                            "value": p.display,
                            "error": p.error,
                        })).collect::<Vec<_>>()
                    })
                })
                .collect();
            Ok(ToolExecutionOutcome {
                output: json!({ "errorCount": rows.len(), "errors": rows }).to_string(),
                committed_seq: None,
                finished: None,
                presence: None,
            })
        }
        "finish" => {
            if sink.engine().has_pending_recalc() {
                sink.engine_mut().recalc_all();
            }
            let summary = args
                .get("summary")
                .and_then(JsonValue::as_str)
                .unwrap_or("Completed requested spreadsheet updates.")
                .trim()
                .to_owned();
            let highlights: Vec<String> = args
                .get("highlights")
                .and_then(JsonValue::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(JsonValue::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let err_count = sink.engine().list_error_cells(None, 20).len();
            Ok(ToolExecutionOutcome {
                output: json!({
                    "status": "finished",
                    "summary": summary,
                    "remainingErrors": err_count
                })
                .to_string(),
                committed_seq: None,
                finished: Some(FinishReport {
                    summary,
                    highlights,
                }),
                presence: None,
            })
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}

async fn exec_write_cells(
    args: &JsonValue,
    sink: &mut dyn AgentWorkbookSink,
) -> Result<ToolExecutionOutcome, String> {
    let sheet_name = args.get("sheet").and_then(JsonValue::as_str);
    let sid = resolve_sheet_id(sink.engine(), sheet_name)?;
    let raw_edits = args
        .get("edits")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "write_cells requires `edits` array".to_owned())?;
    if raw_edits.is_empty() {
        return Err("write_cells `edits` array must not be empty".to_owned());
    }
    if raw_edits.len() > MAX_WRITE_CELLS_PER_CALL {
        return Err(format!(
            "write_cells exceeded cap of {MAX_WRITE_CELLS_PER_CALL} edits per call"
        ));
    }

    let mut edits = Vec::with_capacity(raw_edits.len());
    let mut coords: Vec<(u32, u32, RowId, ColId)> = Vec::with_capacity(raw_edits.len());
    let mut min_r = u32::MAX;
    let mut max_r = 0_u32;
    let mut min_c = u32::MAX;
    let mut max_c = 0_u32;

    for item in raw_edits {
        let cell_str = item
            .get("cell")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| "each edit requires a `cell` address".to_owned())?;
        let addr = parse_cell_addr(cell_str.trim())
            .ok_or_else(|| format!("invalid cell address `{cell_str}`"))?;
        let (row_id, col_id, existing_fmt) = {
            let s = sink
                .engine()
                .workbook()
                .live_sheet(sid)
                .ok_or_else(|| "worksheet missing".to_owned())?;
            let r = s
                .row_axis()
                .pos_to_id(addr.row)
                .ok_or_else(|| format!("row out of bounds for `{cell_str}`"))?;
            let c = s
                .col_axis()
                .pos_to_id(addr.col)
                .ok_or_else(|| format!("col out of bounds for `{cell_str}`"))?;
            (
                RowId(r),
                ColId(c),
                s.effective_format_by_id(RowId(r), ColId(c)),
            )
        };

        let explicit_fmt = item
            .get("number_format")
            .and_then(JsonValue::as_str)
            .map(parse_number_format)
            .transpose()?;

        let (input, inferred_fmt) =
            if let Some(formula_raw) = item.get("formula").and_then(JsonValue::as_str) {
                let trimmed = formula_raw.trim();
                let normalized = if trimmed.starts_with('=') {
                    trimmed.to_owned()
                } else {
                    format!("={trimmed}")
                };
                let bound = BoundFormula::parse_and_bind(
                    &normalized,
                    FormulaMode::Dynamic,
                    sink.engine().workbook(),
                    sid,
                )
                .map_err(|e| format!("invalid formula `{normalized}` at `{cell_str}`: {e:?}"))?;
                (
                    CellInput::Formula {
                        formula: bound,
                        cached: None,
                    },
                    None,
                )
            } else if let Some(val) = item.get("value") {
                json_value_to_cell_input(val, existing_fmt, sink.engine().workbook(), sid)?
            } else {
                (CellInput::Empty, None)
            };

        let format = match explicit_fmt.or(inferred_fmt) {
            Some(fmt) => FormatPatch::Set(fmt),
            None => FormatPatch::Keep,
        };

        min_r = min_r.min(addr.row);
        max_r = max_r.max(addr.row);
        min_c = min_c.min(addr.col);
        max_c = max_c.max(addr.col);
        coords.push((addr.row, addr.col, row_id, col_id));
        edits.push(CellEdit {
            row: row_id,
            col: col_id,
            input,
            format,
        });
    }

    let presence = AgentPresenceHint {
        sheet: sid,
        row: min_r,
        col: min_c,
        end_row: max_r,
        end_col: max_c,
    };
    let count = edits.len();
    let seq = sink
        .commit_ops(
            format!("AI write_cells ({count} cells)"),
            vec![Op::SetCells { sheet: sid, edits }],
            Some(presence),
        )
        .await?;

    let mut samples = Vec::new();
    let mut errors = Vec::new();
    for &(r, c, rid, cid) in &coords {
        if let Some(info) = sink.engine().inspect_cell(sid, rid, cid) {
            if let Some(err) = info.error {
                errors.push(json!({ "cell": a1_of(r, c), "error": err }));
            }
            if samples.len() < 12 {
                samples.push(json!({ "cell": a1_of(r, c), "value": info.display }));
            }
        }
    }
    Ok(ToolExecutionOutcome {
        output: json!({
            "written": count,
            "committedSeq": seq,
            "samples": samples,
            "errors": errors,
        })
        .to_string(),
        committed_seq: Some(seq),
        finished: None,
        presence: Some(presence),
    })
}

async fn exec_fill_formula(
    args: &JsonValue,
    sink: &mut dyn AgentWorkbookSink,
) -> Result<ToolExecutionOutcome, String> {
    let sheet_name = args.get("sheet").and_then(JsonValue::as_str);
    let src_str = args
        .get("source_cell")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "fill_formula requires `source_cell`".to_owned())?;
    let formula_raw = args
        .get("formula")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "fill_formula requires `formula`".to_owned())?;
    let target_range = args
        .get("target_range")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "fill_formula requires `target_range`".to_owned())?;

    let (parsed_sheet, r0, c0, r1, c1) = parse_a1_range(target_range)?;
    let sid = resolve_sheet_id(sink.engine(), parsed_sheet.as_deref().or(sheet_name))?;
    let src_addr = parse_cell_addr(src_str.trim())
        .ok_or_else(|| format!("invalid source_cell `{src_str}`"))?;
    let normalized = if formula_raw.trim().starts_with('=') {
        formula_raw.trim().to_owned()
    } else {
        format!("={}", formula_raw.trim())
    };

    let (template, row_runs, col_runs, cell_count) = {
        let wb = sink.engine().workbook();
        let s = wb
            .live_sheet(sid)
            .ok_or_else(|| "worksheet missing".to_owned())?;
        let src_row = RowId(
            s.row_axis()
                .pos_to_id(src_addr.row)
                .ok_or_else(|| "source row out of bounds".to_owned())?,
        );
        let src_col = ColId(
            s.col_axis()
                .pos_to_id(src_addr.col)
                .ok_or_else(|| "source col out of bounds".to_owned())?,
        );
        let bound = BoundFormula::parse_and_bind(&normalized, FormulaMode::Dynamic, wb, sid)
            .map_err(|e| format!("invalid fill formula `{normalized}`: {e:?}"))?;
        let template = templatize_formula(&bound, src_row, src_col);

        let mut row_runs = Vec::new();
        for r in r0..=r1 {
            let rid = s
                .row_axis()
                .pos_to_id(r)
                .ok_or_else(|| format!("target row {} out of bounds", r + 1))?;
            push_coalesced(&mut row_runs, rid);
        }
        let mut col_runs = Vec::new();
        for c in c0..=c1 {
            let cid = s
                .col_axis()
                .pos_to_id(c)
                .ok_or_else(|| format!("target col {} out of bounds", c + 1))?;
            push_coalesced(&mut col_runs, cid);
        }
        let count = (r1 - r0 + 1) as u64 * (c1 - c0 + 1) as u64;
        (template, row_runs, col_runs, count)
    };

    let presence = AgentPresenceHint {
        sheet: sid,
        row: r0,
        col: c0,
        end_row: r1,
        end_col: c1,
    };
    let seq = sink
        .commit_ops(
            format!("AI fill_formula {target_range}"),
            vec![Op::FillFormula {
                sheet: sid,
                rows: row_runs,
                cols: col_runs,
                template,
            }],
            Some(presence),
        )
        .await?;

    let first_val = sink
        .engine()
        .workbook()
        .live_sheet(sid)
        .and_then(|s| {
            let r = RowId(s.row_axis().pos_to_id(r0)?);
            let c = ColId(s.col_axis().pos_to_id(c0)?);
            sink.engine().inspect_cell(sid, r, c).map(|i| i.display)
        })
        .unwrap_or_default();
    let last_val = sink
        .engine()
        .workbook()
        .live_sheet(sid)
        .and_then(|s| {
            let r = RowId(s.row_axis().pos_to_id(r1)?);
            let c = ColId(s.col_axis().pos_to_id(c1)?);
            sink.engine().inspect_cell(sid, r, c).map(|i| i.display)
        })
        .unwrap_or_default();
    let err_count = sink.engine().list_error_cells(Some(sid), 50).len();

    Ok(ToolExecutionOutcome {
        output: json!({
            "filledCells": cell_count,
            "committedSeq": seq,
            "firstCell": { "cell": a1_of(r0, c0), "value": first_val },
            "lastCell": { "cell": a1_of(r1, c1), "value": last_val },
            "sheetErrorCount": err_count,
        })
        .to_string(),
        committed_seq: Some(seq),
        finished: None,
        presence: Some(presence),
    })
}

async fn exec_edit_structure(
    args: &JsonValue,
    sink: &mut dyn AgentWorkbookSink,
) -> Result<ToolExecutionOutcome, String> {
    let action = args
        .get("action")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "edit_structure requires `action`".to_owned())?;
    let sheet_name = args.get("sheet").and_then(JsonValue::as_str);

    let (op, presence, desc) = match action {
        "insert_rows" => {
            let sid = resolve_sheet_id(sink.engine(), sheet_name)?;
            let idx = args.get("index").and_then(JsonValue::as_u64).unwrap_or(0) as u32;
            let count = args
                .get("count")
                .and_then(JsonValue::as_u64)
                .unwrap_or(1)
                .clamp(1, u64::from(MAX_STRUCT_ROWS_PER_CALL)) as u32;
            let after = {
                let s = sink
                    .engine()
                    .workbook()
                    .live_sheet(sid)
                    .ok_or_else(|| "worksheet missing".to_owned())?;
                if idx == 0 {
                    None
                } else {
                    Some(RowId(s.row_axis().pos_to_id(idx - 1).ok_or_else(|| {
                        "insert row anchor out of bounds".to_owned()
                    })?))
                }
            };
            let ids = sink.alloc_row_run(count)?;
            (
                Op::InsertRows {
                    sheet: sid,
                    after,
                    ids,
                },
                Some(AgentPresenceHint {
                    sheet: sid,
                    row: idx,
                    col: 0,
                    end_row: idx + count - 1,
                    end_col: 0,
                }),
                format!("Inserted {count} row(s) at row {}", idx + 1),
            )
        }
        "delete_rows" => {
            let sid = resolve_sheet_id(sink.engine(), sheet_name)?;
            let idx = args.get("index").and_then(JsonValue::as_u64).unwrap_or(0) as u32;
            let count = args
                .get("count")
                .and_then(JsonValue::as_u64)
                .unwrap_or(1)
                .clamp(1, u64::from(MAX_STRUCT_ROWS_PER_CALL)) as u32;
            let mut runs = Vec::new();
            {
                let s = sink
                    .engine()
                    .workbook()
                    .live_sheet(sid)
                    .ok_or_else(|| "worksheet missing".to_owned())?;
                for r in idx..idx.saturating_add(count) {
                    let rid = s
                        .row_axis()
                        .pos_to_id(r)
                        .ok_or_else(|| format!("row {} out of bounds", r + 1))?;
                    push_coalesced(&mut runs, rid);
                }
            }
            (
                Op::DeleteRows {
                    sheet: sid,
                    ids: runs,
                },
                Some(AgentPresenceHint {
                    sheet: sid,
                    row: idx,
                    col: 0,
                    end_row: idx,
                    end_col: 0,
                }),
                format!("Deleted {count} row(s) starting at row {}", idx + 1),
            )
        }
        "insert_cols" => {
            let sid = resolve_sheet_id(sink.engine(), sheet_name)?;
            let idx = args.get("index").and_then(JsonValue::as_u64).unwrap_or(0) as u32;
            let count = args
                .get("count")
                .and_then(JsonValue::as_u64)
                .unwrap_or(1)
                .clamp(1, u64::from(MAX_STRUCT_COLS_PER_CALL)) as u32;
            let after = {
                let s = sink
                    .engine()
                    .workbook()
                    .live_sheet(sid)
                    .ok_or_else(|| "worksheet missing".to_owned())?;
                if idx == 0 {
                    None
                } else {
                    Some(ColId(s.col_axis().pos_to_id(idx - 1).ok_or_else(|| {
                        "insert col anchor out of bounds".to_owned()
                    })?))
                }
            };
            let ids = sink.alloc_col_run(count)?;
            (
                Op::InsertCols {
                    sheet: sid,
                    after,
                    ids,
                },
                Some(AgentPresenceHint {
                    sheet: sid,
                    row: 0,
                    col: idx,
                    end_row: 0,
                    end_col: idx + count - 1,
                }),
                format!("Inserted {count} column(s) at col {}", idx + 1),
            )
        }
        "delete_cols" => {
            let sid = resolve_sheet_id(sink.engine(), sheet_name)?;
            let idx = args.get("index").and_then(JsonValue::as_u64).unwrap_or(0) as u32;
            let count = args
                .get("count")
                .and_then(JsonValue::as_u64)
                .unwrap_or(1)
                .clamp(1, u64::from(MAX_STRUCT_COLS_PER_CALL)) as u32;
            let mut runs = Vec::new();
            {
                let s = sink
                    .engine()
                    .workbook()
                    .live_sheet(sid)
                    .ok_or_else(|| "worksheet missing".to_owned())?;
                for c in idx..idx.saturating_add(count) {
                    let cid = s
                        .col_axis()
                        .pos_to_id(c)
                        .ok_or_else(|| format!("col {} out of bounds", c + 1))?;
                    push_coalesced(&mut runs, cid);
                }
            }
            (
                Op::DeleteCols {
                    sheet: sid,
                    ids: runs,
                },
                Some(AgentPresenceHint {
                    sheet: sid,
                    row: 0,
                    col: idx,
                    end_row: 0,
                    end_col: idx,
                }),
                format!("Deleted {count} column(s) starting at col {}", idx + 1),
            )
        }
        "add_sheet" => {
            let name = args
                .get("name")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "add_sheet requires `name`".to_owned())?
                .trim()
                .to_owned();
            let after = sink
                .engine()
                .workbook()
                .live_sheets()
                .last()
                .map(reticula_core::sheet::Sheet::id);
            let sid = sink.alloc_sheet_id()?;
            (
                Op::AddSheet {
                    sheet: sid,
                    name: name.clone(),
                    after,
                },
                Some(AgentPresenceHint {
                    sheet: sid,
                    row: 0,
                    col: 0,
                    end_row: 0,
                    end_col: 0,
                }),
                format!("Added worksheet '{name}'"),
            )
        }
        "rename_sheet" => {
            let sid = resolve_sheet_id(sink.engine(), sheet_name)?;
            let name = args
                .get("name")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "rename_sheet requires `name`".to_owned())?
                .trim()
                .to_owned();
            (
                Op::RenameSheet {
                    sheet: sid,
                    name: name.clone(),
                },
                None,
                format!("Renamed worksheet to '{name}'"),
            )
        }
        "delete_sheet" => {
            let sid = resolve_sheet_id(sink.engine(), sheet_name)?;
            (
                Op::DeleteSheet { sheet: sid },
                None,
                "Deleted worksheet".to_owned(),
            )
        }
        "move_rows" => {
            let sid = resolve_sheet_id(sink.engine(), sheet_name)?;
            let idx = args.get("index").and_then(JsonValue::as_u64).unwrap_or(0) as u32;
            let count = args.get("count").and_then(JsonValue::as_u64).unwrap_or(1) as u32;
            let target = args
                .get("target_index")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| "move_rows requires `target_index`".to_owned())?
                as u32;
            let (runs, after) = {
                let s = sink
                    .engine()
                    .workbook()
                    .live_sheet(sid)
                    .ok_or_else(|| "worksheet missing".to_owned())?;
                let mut runs = Vec::new();
                for r in idx..idx.saturating_add(count) {
                    let rid = s
                        .row_axis()
                        .pos_to_id(r)
                        .ok_or_else(|| "move_rows index out of bounds".to_owned())?;
                    push_coalesced(&mut runs, rid);
                }
                let after = if target == 0 {
                    None
                } else {
                    Some(RowId(s.row_axis().pos_to_id(target - 1).ok_or_else(
                        || "move_rows target out of bounds".to_owned(),
                    )?))
                };
                (runs, after)
            };
            (
                Op::MoveRows {
                    sheet: sid,
                    ids: runs,
                    after,
                },
                None,
                format!("Moved {count} row(s)"),
            )
        }
        "move_cols" => {
            let sid = resolve_sheet_id(sink.engine(), sheet_name)?;
            let idx = args.get("index").and_then(JsonValue::as_u64).unwrap_or(0) as u32;
            let count = args.get("count").and_then(JsonValue::as_u64).unwrap_or(1) as u32;
            let target = args
                .get("target_index")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| "move_cols requires `target_index`".to_owned())?
                as u32;
            let (runs, after) = {
                let s = sink
                    .engine()
                    .workbook()
                    .live_sheet(sid)
                    .ok_or_else(|| "worksheet missing".to_owned())?;
                let mut runs = Vec::new();
                for c in idx..idx.saturating_add(count) {
                    let cid = s
                        .col_axis()
                        .pos_to_id(c)
                        .ok_or_else(|| "move_cols index out of bounds".to_owned())?;
                    push_coalesced(&mut runs, cid);
                }
                let after = if target == 0 {
                    None
                } else {
                    Some(ColId(s.col_axis().pos_to_id(target - 1).ok_or_else(
                        || "move_cols target out of bounds".to_owned(),
                    )?))
                };
                (runs, after)
            };
            (
                Op::MoveCols {
                    sheet: sid,
                    ids: runs,
                    after,
                },
                None,
                format!("Moved {count} column(s)"),
            )
        }
        "define_named_range" => {
            let name = args
                .get("name")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "define_named_range requires `name`".to_owned())?
                .trim()
                .to_owned();
            let formula_raw = args
                .get("formula")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "define_named_range requires `formula`".to_owned())?;
            let workbook_scope = args
                .get("workbook_scope")
                .and_then(JsonValue::as_bool)
                .unwrap_or(true);
            let host = resolve_sheet_id(sink.engine(), sheet_name)?;
            let normalized = if formula_raw.trim().starts_with('=') {
                formula_raw.trim().to_owned()
            } else {
                format!("={}", formula_raw.trim())
            };
            let formula = BoundFormula::parse_and_bind(
                &normalized,
                FormulaMode::Dynamic,
                sink.engine().workbook(),
                host,
            )
            .map_err(|e| format!("invalid named range formula `{normalized}`: {e:?}"))?;
            (
                Op::DefineNamedRange {
                    scope: if workbook_scope { None } else { Some(host) },
                    name: name.clone(),
                    formula,
                    comment: None,
                    hidden: false,
                },
                None,
                format!("Defined named range '{name}' = {normalized}"),
            )
        }
        "delete_named_range" => {
            let name = args
                .get("name")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "delete_named_range requires `name`".to_owned())?
                .trim()
                .to_owned();
            let workbook_scope = args
                .get("workbook_scope")
                .and_then(JsonValue::as_bool)
                .unwrap_or(true);
            let scope = if workbook_scope {
                None
            } else {
                Some(resolve_sheet_id(sink.engine(), sheet_name)?)
            };
            (
                Op::DeleteNamedRange {
                    scope,
                    name: name.clone(),
                },
                None,
                format!("Deleted named range '{name}'"),
            )
        }
        other => return Err(format!("unsupported edit_structure action `{other}`")),
    };

    let seq = sink
        .commit_ops(format!("AI {action}"), vec![op], presence)
        .await?;
    Ok(ToolExecutionOutcome {
        output: json!({ "ok": true, "committedSeq": seq, "detail": desc }).to_string(),
        committed_seq: Some(seq),
        finished: None,
        presence,
    })
}

async fn exec_sort_range(
    args: &JsonValue,
    sink: &mut dyn AgentWorkbookSink,
) -> Result<ToolExecutionOutcome, String> {
    let sheet_name = args.get("sheet").and_then(JsonValue::as_str);
    let range = args
        .get("range")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "sort_range requires `range`".to_owned())?;
    let has_header = args
        .get("has_header")
        .and_then(JsonValue::as_bool)
        .unwrap_or(true);
    let raw_keys = args
        .get("keys")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "sort_range requires `keys` array".to_owned())?;
    let mut keys = Vec::new();
    for k in raw_keys {
        let col_offset = k
            .get("column")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| "each sort key requires `column` offset".to_owned())?
            as u32;
        let descending = k
            .get("descending")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        keys.push(SortKey {
            col_offset,
            ascending: !descending,
            case_sensitive: false,
        });
    }

    let (parsed_sheet, r0, c0, r1, c1) = parse_a1_range(range)?;
    let sid = resolve_sheet_id(sink.engine(), parsed_sheet.as_deref().or(sheet_name))?;
    let op = {
        let s = sink
            .engine()
            .workbook()
            .live_sheet(sid)
            .ok_or_else(|| "worksheet missing".to_owned())?;
        plan_sort(s, r0, r1, c0, c1, has_header, &keys, None)
    };
    let presence = AgentPresenceHint {
        sheet: sid,
        row: r0,
        col: c0,
        end_row: r1,
        end_col: c1,
    };
    let Some(sort_op) = op else {
        return Ok(ToolExecutionOutcome {
            output:
                json!({ "sortedRows": 0, "note": "range already sorted or fewer than 2 data rows" })
                    .to_string(),
            committed_seq: None,
            finished: None,
            presence: Some(presence),
        });
    };
    let row_count = match &sort_op {
        Op::PermuteContent { rows, .. } => rows.len(),
        _ => 0,
    };
    let seq = sink
        .commit_ops(
            format!("AI sort_range {range}"),
            vec![sort_op],
            Some(presence),
        )
        .await?;
    Ok(ToolExecutionOutcome {
        output: json!({ "sortedRows": row_count, "committedSeq": seq }).to_string(),
        committed_seq: Some(seq),
        finished: None,
        presence: Some(presence),
    })
}

async fn exec_format_range(
    args: &JsonValue,
    sink: &mut dyn AgentWorkbookSink,
) -> Result<ToolExecutionOutcome, String> {
    let sheet_name = args.get("sheet").and_then(JsonValue::as_str);
    let range = args
        .get("range")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "format_range requires `range`".to_owned())?;
    let (parsed_sheet, r0, c0, r1, c1) = parse_a1_range(range)?;
    let sid = resolve_sheet_id(sink.engine(), parsed_sheet.as_deref().or(sheet_name))?;

    let number_format = args
        .get("number_format")
        .and_then(JsonValue::as_str)
        .map(parse_number_format)
        .transpose()?;
    let style = args.get("style").map(parse_style_patch).transpose()?;
    if number_format.is_none() && style.is_none() {
        return Err("format_range requires `number_format` and/or `style`".to_owned());
    }

    let (row_runs, col_runs, cell_count) = {
        let s = sink
            .engine()
            .workbook()
            .live_sheet(sid)
            .ok_or_else(|| "worksheet missing".to_owned())?;
        let mut row_runs = Vec::new();
        for r in r0..=r1 {
            let rid = s
                .row_axis()
                .pos_to_id(r)
                .ok_or_else(|| "format_range row out of bounds".to_owned())?;
            push_coalesced(&mut row_runs, rid);
        }
        let mut col_runs = Vec::new();
        for c in c0..=c1 {
            let cid = s
                .col_axis()
                .pos_to_id(c)
                .ok_or_else(|| "format_range col out of bounds".to_owned())?;
            push_coalesced(&mut col_runs, cid);
        }
        (
            row_runs,
            col_runs,
            (r1 - r0 + 1) as u64 * (c1 - c0 + 1) as u64,
        )
    };

    let presence = AgentPresenceHint {
        sheet: sid,
        row: r0,
        col: c0,
        end_row: r1,
        end_col: c1,
    };
    let seq = sink
        .commit_ops(
            format!("AI format_range {range}"),
            vec![Op::SetFormat {
                sheet: sid,
                target: FormatTarget::Cells {
                    rows: row_runs,
                    cols: col_runs,
                },
                number_format: number_format.map_or(FormatPatch::Keep, FormatPatch::Set),
                style: style.unwrap_or_default(),
            }],
            Some(presence),
        )
        .await?;
    Ok(ToolExecutionOutcome {
        output: json!({ "formattedCells": cell_count, "committedSeq": seq }).to_string(),
        committed_seq: Some(seq),
        finished: None,
        presence: Some(presence),
    })
}

fn json_value_to_cell_input(
    val: &JsonValue,
    existing_fmt: NumberFormat,
    wb: &Workbook<BoundFormula>,
    sid: SheetId,
) -> Result<(CellInput<BoundFormula>, Option<NumberFormat>), String> {
    match val {
        JsonValue::Null => Ok((CellInput::Empty, None)),
        JsonValue::Bool(b) => Ok((CellInput::Scalar(ScalarValue::Bool(*b)), None)),
        JsonValue::Number(n) => {
            let f = n.as_f64().ok_or_else(|| "non-finite number".to_owned())?;
            Ok((CellInput::Scalar(ScalarValue::Number(f)), None))
        }
        JsonValue::String(s) => {
            if s.trim_start().starts_with('=') {
                let bound = BoundFormula::parse_and_bind(s.trim(), FormulaMode::Dynamic, wb, sid)
                    .map_err(|e| format!("invalid formula `{s}`: {e:?}"))?;
                return Ok((
                    CellInput::Formula {
                        formula: bound,
                        cached: None,
                    },
                    None,
                ));
            }
            match infer_input_for_cell(s, existing_fmt) {
                Ok(InferredInput::Empty) => Ok((CellInput::Empty, None)),
                Ok(InferredInput::Scalar {
                    value,
                    suggested_format,
                }) => Ok((CellInput::Scalar(value), suggested_format)),
                Ok(InferredInput::Formula(raw)) => {
                    let bound = BoundFormula::parse_and_bind(&raw.source, raw.mode, wb, sid)
                        .map_err(|e| format!("invalid formula `{}`: {e:?}", raw.source))?;
                    Ok((
                        CellInput::Formula {
                            formula: bound,
                            cached: None,
                        },
                        None,
                    ))
                }
                Err(_) => Ok((CellInput::Scalar(ScalarValue::Text(s.clone())), None)),
            }
        }
        other => Ok((
            CellInput::Scalar(ScalarValue::Text(other.to_string())),
            None,
        )),
    }
}

fn parse_number_format(raw: &str) -> Result<NumberFormat, String> {
    let key = raw.trim().to_ascii_lowercase();
    match key.as_str() {
        "general" | "" => Ok(NumberFormat::General),
        "integer" | "0" => Ok(NumberFormat::Integer),
        "fixed2" | "0.00" => Ok(NumberFormat::Fixed2),
        "#,##0" => Ok(NumberFormat::Fixed {
            decimals: 0,
            thousands: true,
        }),
        "#,##0.00" | "number" => Ok(NumberFormat::Fixed {
            decimals: 2,
            thousands: true,
        }),
        "percent" | "0%" => Ok(NumberFormat::Percent),
        "percent2" | "0.0%" | "0.00%" => Ok(NumberFormat::PercentDecimals { decimals: 2 }),
        "currency" | "usd" | "$#,##0.00" => Ok(NumberFormat::Currency {
            symbol: CurrencySymbol::Usd,
            decimals: 2,
        }),
        "currency0" | "$#,##0" => Ok(NumberFormat::Currency {
            symbol: CurrencySymbol::Usd,
            decimals: 0,
        }),
        "eur" => Ok(NumberFormat::Currency {
            symbol: CurrencySymbol::Eur,
            decimals: 2,
        }),
        "cny" => Ok(NumberFormat::Currency {
            symbol: CurrencySymbol::Cny,
            decimals: 2,
        }),
        "date" | "yyyy-mm-dd" => Ok(NumberFormat::Date),
        "text" | "@" => Ok(NumberFormat::PlainText),
        other => Err(format!("unsupported number_format `{other}`")),
    }
}

fn parse_style_patch(v: &JsonValue) -> Result<StylePatch, String> {
    let mut patch = StylePatch::default();
    if let Some(b) = v.get("bold").and_then(JsonValue::as_bool) {
        patch.bold = Some(b);
    }
    if let Some(i) = v.get("italic").and_then(JsonValue::as_bool) {
        patch.italic = Some(i);
    }
    if let Some(align) = v.get("align").and_then(JsonValue::as_str) {
        patch.align = Some(match align.to_ascii_lowercase().as_str() {
            "left" => HorizontalAlign::Left,
            "center" => HorizontalAlign::Center,
            "right" => HorizontalAlign::Right,
            other => return Err(format!("unsupported align `{other}`")),
        });
    }
    if let Some(hex) = v.get("fill_color").and_then(JsonValue::as_str) {
        patch.fill_color = ColorPatch::Set(ColorValue::Rgb(parse_hex_rgb(hex)?));
    }
    if let Some(hex) = v.get("text_color").and_then(JsonValue::as_str) {
        patch.text_color = ColorPatch::Set(ColorValue::Rgb(parse_hex_rgb(hex)?));
    }
    Ok(patch)
}

fn parse_hex_rgb(raw: &str) -> Result<u32, String> {
    let cleaned = raw.trim().trim_start_matches('#');
    if cleaned.len() != 6 {
        return Err(format!("invalid RGB hex color `{raw}`"));
    }
    u32::from_str_radix(cleaned, 16).map_err(|_| format!("invalid RGB hex color `{raw}`"))
}

fn eval_scalar_to_json(s: &EvalScalar) -> JsonValue {
    match s {
        EvalScalar::Empty => JsonValue::Null,
        EvalScalar::Number(n) => json!(n),
        EvalScalar::Text(t) => json!(t),
        EvalScalar::Bool(b) => json!(b),
        EvalScalar::Error(e) => json!(e.as_str()),
    }
}

fn push_coalesced(out: &mut Vec<IdRun>, raw_id: u64) {
    if let Some(last) = out.last_mut()
        && let Some(next_id) = last
            .id_at(last.len.saturating_sub(1))
            .and_then(|v| v.checked_add(1))
        && next_id == raw_id
        && reticula_core::id::id_stream(last.first_id) == reticula_core::id::id_stream(raw_id)
        && let Some(new_len) = last.len.checked_add(1)
        && IdRun::new(last.first_id, new_len).is_some()
    {
        last.len = new_len;
        return;
    }
    if let Some(single) = IdRun::new(raw_id, 1) {
        out.push(single);
    }
}

/// Checks whether a workbook cell currently holds a calculation error.
pub fn cell_error_text(val: Value) -> Option<&'static str> {
    match val {
        Value::Error(e) => Some(e.as_str()),
        Value::Array(_) => Some("#SPILL!"),
        _ => None,
    }
}
