//! Three-way cell & structural diff engine, static reference closure checker,
//! and `RemapState`-backed selective merge planner.

use std::collections::{BTreeMap, BTreeSet};

use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdAllocator, IdRun, RowId, SheetId};
use reticula_core::ops::{
    CellEdit, CellInput, FormatPatch, FormatTarget, NamedRangeKey, Op, ScalarValue, Workbook,
};
use reticula_core::sheet::Sheet;
use reticula_core::style::StylePatch;
use reticula_core::value::Value;
use reticula_formula::address::{CellAddr, write_col_letters};
use reticula_formula::binder::{BoundExpr, BoundFormula, BoundRef};
use reticula_formula::engine::FormulaEngine;
use reticula_sync::RemapState;
use reticula_wire::convert_v3::ops_from_wire_txn_v3;
use reticula_wire::v3::WireCommittedTxnV3;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Maximum `Op::SetCells` edits emitted per sharded merge operation (well below `MAX_EDITS_PER_OP = 10_000`).
const MERGE_SET_CELLS_CHUNK: usize = 2_000;

/// Category of a structured diff hunk shown in the review drawer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiffHunkKind {
    /// Atomic structural operation (`AddSheet`, `InsertRows`, `DeleteRows`, etc.)
    /// bundled with all edits targeting newly created sheets/rows/columns.
    Structural,
    /// Content-only sort (`Op::PermuteContent`) folded into a single review unit.
    Sort,
    /// Contiguous block of cell value/formula edits on pre-existing rows & columns.
    CellBlock,
    /// Named ranges, row/column sizes, freeze panes, or pure format/style updates.
    Meta,
}

/// Resolution choice for a three-way conflicting cell or hunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolution {
    TakeDraft,
    KeepMain,
}

/// Summary metadata for a folded `SortHunk`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SortHunkSummary {
    pub range: String,
    pub row_count: u32,
    pub col_count: u32,
}

/// Single cell-level entry inside a diff hunk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CellDiffEntry {
    /// Stable cell key (`"<sheet_id>:<row_id>:<col_id>"`) used in `conflictChoices`.
    pub cell_key: String,
    pub sheet_name: String,
    pub address: String,
    pub row_pos: u32,
    pub col_pos: u32,
    pub base_value: String,
    pub base_formula: Option<String>,
    pub draft_value: String,
    pub draft_formula: Option<String>,
    pub main_value: String,
    pub main_formula: Option<String>,
    pub conflict: bool,
    pub default_resolution: ConflictResolution,
}

/// Read-only notification of an unchanged formula cell whose evaluated result
/// cascaded because of an upstream data edit (never an editable conflict).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CascadedCell {
    pub sheet_name: String,
    pub address: String,
    pub base_value: String,
    pub draft_value: String,
    pub formula: String,
}

/// Single reviewable diff unit presented to the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    pub id: String,
    pub kind: DiffHunkKind,
    pub sheet_id: String,
    pub sheet_name: String,
    pub label: String,
    pub default_accepted: bool,
    pub has_conflict: bool,
    pub conflict_reason: Option<String>,
    pub anchor_cell: Option<String>,
    pub cells: Vec<CellDiffEntry>,
    pub structural_op_summary: Option<String>,
    pub sort_summary: Option<SortHunkSummary>,
}

/// High-level summary counts for a branch diff.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiffSummary {
    pub total_hunks: u32,
    pub structural_hunks: u32,
    pub sort_hunks: u32,
    pub cell_block_hunks: u32,
    pub meta_hunks: u32,
    pub conflict_cells: u32,
    pub changed_cells: u32,
    pub cascaded_cells: u32,
}

/// Complete serializable three-way diff payload returned by `GET .../diff`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BranchDiff {
    pub fork_seq: String,
    pub main_head_seq: String,
    pub hunks: Vec<DiffHunk>,
    pub cascaded_cells: Vec<CascadedCell>,
    pub summary: DiffSummary,
}

/// Static reference closure warning: an accepted formula references a newly
/// inserted sheet/row/column belonging to a rejected structural hunk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClosureIssue {
    pub source_cell: String,
    pub formula_text: String,
    pub target_hunk_id: String,
    pub target_hunk_label: String,
    pub ref_display: String,
}

/// User selection sent to `POST .../merge/preview` and `POST .../merge`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MergeSelection {
    pub accepted_hunks: Vec<String>,
    #[serde(default)]
    pub conflict_choices: BTreeMap<String, ConflictResolution>,
    #[serde(default)]
    pub allow_ref_errors: bool,
    #[serde(default)]
    pub expected_main_seq: Option<String>,
}

/// Response returned by `POST .../merge/preview`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MergePreviewResponse {
    pub can_merge: bool,
    pub main_head_seq: String,
    pub closure_issues: Vec<ClosureIssue>,
    pub unresolved_conflicts: Vec<String>,
    pub estimated_ops: u32,
}

/// Error returned when planning merge operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergePlanError {
    /// Accepted formulas reference newly created IDs from rejected structural hunks.
    ClosureBlocked(Vec<ClosureIssue>),
    /// One or more accepted conflict cells had no explicit resolution.
    UnresolvedConflicts(Vec<String>),
    /// `RemapState` exhausted fresh ID counters or encountered an invalid op.
    RemapFailed(String),
}

/// Internal per-cell logical edit prepared for merge synthesis.
#[derive(Clone, Debug)]
struct PlannedCellChange {
    cell_key: String,
    sheet: SheetId,
    row: RowId,
    col: ColId,
    address: String,
    sheet_name: String,
    formula_text: Option<String>,
    input: CellInput<BoundFormula>,
    format: FormatPatch,
    style: Option<StylePatch>,
    conflict: bool,
    deleted_on_main: bool,
}

/// Internal per-hunk operational payload paired with [`DiffHunk`].
#[derive(Clone, Debug)]
struct PlannedHunk {
    hunk_id: String,
    label: String,
    kind: DiffHunkKind,
    /// Structural ops (in commit order) belonging to this hunk.
    structural_ops: Vec<Op<BoundFormula>>,
    /// Sort op belonging to a `SortHunk`.
    sort_op: Option<Op<BoundFormula>>,
    /// Newly introduced sheet/row/col IDs owned by this `StructuralHunk`.
    created_sheets: BTreeSet<SheetId>,
    created_rows: BTreeSet<(SheetId, RowId)>,
    created_cols: BTreeSet<(SheetId, ColId)>,
    /// Cell edits belonging to this hunk (for `Structural` new cells or `CellBlock`).
    cell_changes: Vec<PlannedCellChange>,
    /// Meta ops (`SetFormat`, `DefineNamedRange`, `DeleteNamedRange`, `Resize*`, `SetFreeze`).
    meta_ops: Vec<Op<BoundFormula>>,
}

type SortInverseEntry = (SheetId, BTreeSet<ColId>, BTreeMap<RowId, RowId>);
type SortInverseMap = BTreeMap<String, SortInverseEntry>;
type ExistingCellChange = (u32, u32, CellDiffEntry, PlannedCellChange, bool);

/// Full diff + merge planning context produced by [`compute_branch_diff`].
#[derive(Clone, Debug)]
pub struct BranchDiffPlan {
    pub diff: BranchDiff,
    hunks: Vec<PlannedHunk>,
    /// Named sheet lookup from the draft workbook for human-readable `#REF!` labels.
    draft_sheet_names: BTreeMap<SheetId, String>,
    /// 0-based row positions in the draft workbook for human-readable closure alerts.
    draft_row_pos: BTreeMap<(SheetId, RowId), u32>,
    /// 0-based column positions in the draft workbook for human-readable closure alerts.
    draft_col_pos: BTreeMap<(SheetId, ColId), u32>,
    /// Unparsed named range formulas from draft keyed by hunk ID.
    named_range_formula_texts: BTreeMap<String, String>,
    /// Pre-sort to post-sort row mapping for each `SortHunk`, allowing post-sort
    /// edits on a rejected sort to remap cleanly back to the original row identity.
    sort_inverse_rows: SortInverseMap,
}

/// Logical representation of a cell's authored state (ignoring volatile cached
/// recalculation values to enforce E21).
#[derive(Clone, Debug, PartialEq)]
struct LogicalCell {
    content: LogicalContent,
    format: Option<NumberFormat>,
    style: StylePatch,
}

#[derive(Clone, Debug, PartialEq)]
enum LogicalContent {
    Empty,
    Scalar(ScalarValue),
    Formula(BoundFormula),
}

impl LogicalContent {
    fn from_workbook(wb: &Workbook<BoundFormula>, sheet: &Sheet, row: RowId, col: ColId) -> Self {
        if let Some(formula) = wb.formula(sheet.id(), row, col) {
            return Self::Formula(formula.into_owned());
        }
        match sheet.value_by_id(row, col) {
            Value::Empty => Self::Empty,
            Value::Number(n) => Self::Scalar(ScalarValue::Number(n)),
            Value::Bool(b) => Self::Scalar(ScalarValue::Bool(b)),
            Value::Error(e) => Self::Scalar(ScalarValue::Error(e)),
            Value::Text(id) => Self::Scalar(ScalarValue::Text(sheet.strings().get(id).to_owned())),
            Value::Array(_) => Self::Empty,
        }
    }

    fn to_cell_input(&self) -> CellInput<BoundFormula> {
        match self {
            Self::Empty => CellInput::Empty,
            Self::Scalar(s) => CellInput::Scalar(s.clone()),
            Self::Formula(f) => CellInput::Formula {
                formula: f.clone(),
                cached: None,
            },
        }
    }
}

fn logical_cell(
    wb: &Workbook<BoundFormula>,
    sheet_id: SheetId,
    row: RowId,
    col: ColId,
) -> LogicalCell {
    let Some(sheet) = wb.sheet(sheet_id) else {
        return LogicalCell {
            content: LogicalContent::Empty,
            format: None,
            style: StylePatch::default(),
        };
    };
    LogicalCell {
        content: LogicalContent::from_workbook(wb, sheet, row, col),
        format: sheet.cell_format_by_id(row, col),
        style: sheet.cell_style_patch(row, col),
    }
}

fn format_display_and_formula(
    engine: &FormulaEngine,
    sheet_id: SheetId,
    row: RowId,
    col: ColId,
) -> (String, Option<String>) {
    let wb = engine.workbook();
    let Some(sheet) = wb.sheet(sheet_id) else {
        return (String::new(), None);
    };
    let val = sheet.value_by_id(row, col);
    let mut display = String::new();
    sheet.format_value(val, row, col, &mut display);
    let formula = wb
        .formula(sheet_id, row, col)
        .map(|f| f.unparse(wb, sheet_id));
    (display, formula)
}

/// Formats a canonical stable cell key `"<sheet_id>:<row_id>:<col_id>"`.
pub fn format_cell_key(sheet: SheetId, row: RowId, col: ColId) -> String {
    format!("{}:{}:{}", sheet.0, row.0, col.0)
}

fn format_a1(row_pos: u32, col_pos: u32) -> String {
    let mut out = String::new();
    CellAddr {
        col: col_pos,
        row: row_pos,
        abs_col: false,
        abs_row: false,
    }
    .write_a1(&mut out);
    out
}

fn collect_sheet_populated_keys(
    wb: &Workbook<BoundFormula>,
    sheet: &Sheet,
) -> BTreeSet<(RowId, ColId)> {
    let mut keys = BTreeSet::new();
    sheet.for_each_non_empty_cell(|_, _, r, c, _| {
        keys.insert((r, c));
    });
    for (sid, r, c) in wb.formulas().keys() {
        if sid == sheet.id() {
            keys.insert((r, c));
        }
    }
    for &(r, c) in sheet.cell_styles().keys() {
        keys.insert((r, c));
    }
    keys
}

/// Computes the deterministic three-way diff and merge plan between `base` (at `fork_seq`),
/// `draft` (the AI draft branch head), and `main_head` (at `main_head_seq`).
pub fn compute_branch_diff(
    base: &Workbook<BoundFormula>,
    draft: &Workbook<BoundFormula>,
    main_head: &Workbook<BoundFormula>,
    fork_seq: u64,
    main_head_seq: u64,
    draft_txns: &[WireCommittedTxnV3],
) -> BranchDiffPlan {
    // Recalculate deterministic display views without mutating caller workbooks.
    let base_eval = FormulaEngine::from_workbook_recalc_all(base.clone());
    let draft_eval = FormulaEngine::from_workbook_recalc_all(draft.clone());
    let main_eval = FormulaEngine::from_workbook_recalc_all(main_head.clone());

    let mut draft_sheet_names = BTreeMap::new();
    for s in draft.sheets() {
        draft_sheet_names.insert(s.id(), s.name().to_owned());
    }
    for s in base.sheets() {
        draft_sheet_names
            .entry(s.id())
            .or_insert_with(|| s.name().to_owned());
    }

    // Step 1: Decode draft transactions to isolate structural operations, E15 sorts,
    // and any explicitly formatted empty cells.
    let mut raw_structural_ops: Vec<Op<BoundFormula>> = Vec::new();
    let mut raw_sort_ops: Vec<Op<BoundFormula>> = Vec::new();
    let mut format_touched_keys: BTreeMap<SheetId, BTreeSet<(RowId, ColId)>> = BTreeMap::new();
    let mut base_after_sorts = base.clone();
    let mut main_after_sorts = main_head.clone();

    for committed in draft_txns {
        if let Ok((_, ops)) = ops_from_wire_txn_v3(committed.txn.clone()) {
            for op in ops {
                match &op {
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
                        raw_structural_ops.push(op);
                    }
                    Op::PermuteContent { .. } => {
                        let _ = base_after_sorts.apply(op.clone());
                        let _ = main_after_sorts.apply(op.clone());
                        raw_sort_ops.push(op);
                    }
                    Op::SetFormat {
                        sheet,
                        target: FormatTarget::Cells { rows, cols },
                        ..
                    } => {
                        let entry = format_touched_keys.entry(*sheet).or_default();
                        for r in rows.iter().flat_map(|run| run.iter()) {
                            for c in cols.iter().flat_map(|run| run.iter()) {
                                entry.insert((RowId(r), ColId(c)));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    let base_after_sorts_eval = FormulaEngine::from_workbook_recalc_all(base_after_sorts.clone());
    let main_after_sorts_eval = FormulaEngine::from_workbook_recalc_all(main_after_sorts.clone());

    let mut draft_row_pos: BTreeMap<(SheetId, RowId), u32> = BTreeMap::new();
    let mut draft_col_pos: BTreeMap<(SheetId, ColId), u32> = BTreeMap::new();
    let mut named_range_formula_texts: BTreeMap<String, String> = BTreeMap::new();

    let mut public_hunks: Vec<DiffHunk> = Vec::new();
    let mut planned_hunks: Vec<PlannedHunk> = Vec::new();
    let mut hunk_counter: u32 = 0;
    let mut next_hunk_id = |prefix: &str| {
        hunk_counter += 1;
        format!("{prefix}-{hunk_counter}")
    };

    // Track ownership of newly created sheets/rows/cols -> index into `public_hunks` / `planned_hunks`.
    let mut new_sheet_owner: BTreeMap<SheetId, usize> = BTreeMap::new();
    let mut new_row_owner: BTreeMap<(SheetId, RowId), usize> = BTreeMap::new();
    let mut new_col_owner: BTreeMap<(SheetId, ColId), usize> = BTreeMap::new();

    // Step 2: Materialize StructuralHunks in commit order.
    for op in raw_structural_ops {
        let hunk_id = next_hunk_id("struct");
        let (
            sheet_id,
            label,
            summary,
            created_sheets,
            created_rows,
            created_cols,
            conflict_reason,
            conflict_cells,
        ) = describe_structural_op(
            &op,
            base,
            draft,
            main_head,
            &base_eval,
            &main_eval,
            &draft_sheet_names,
        );
        let sheet_name = draft_sheet_names
            .get(&sheet_id)
            .cloned()
            .unwrap_or_else(|| format!("Sheet#{}", sheet_id.0));
        let idx = public_hunks.len();
        for &sid in &created_sheets {
            new_sheet_owner.insert(sid, idx);
        }
        for &key in &created_rows {
            new_row_owner.insert(key, idx);
            if let Some(pos) = draft
                .sheet(key.0)
                .and_then(|s| s.row_axis().id_to_pos(key.1.0))
            {
                draft_row_pos.insert(key, pos);
            }
        }
        for &key in &created_cols {
            new_col_owner.insert(key, idx);
            if let Some(pos) = draft
                .sheet(key.0)
                .and_then(|s| s.col_axis().id_to_pos(key.1.0))
            {
                draft_col_pos.insert(key, pos);
            }
        }
        let has_conflict = conflict_reason.is_some() || !conflict_cells.is_empty();
        let anchor_cell = conflict_cells.first().map(|c| c.address.clone());
        public_hunks.push(DiffHunk {
            id: hunk_id.clone(),
            kind: DiffHunkKind::Structural,
            sheet_id: sheet_id.0.to_string(),
            sheet_name,
            label: label.clone(),
            default_accepted: !has_conflict,
            has_conflict,
            conflict_reason,
            anchor_cell,
            cells: conflict_cells,
            structural_op_summary: Some(summary),
            sort_summary: None,
        });
        planned_hunks.push(PlannedHunk {
            hunk_id,
            label,
            kind: DiffHunkKind::Structural,
            structural_ops: vec![op],
            sort_op: None,
            created_sheets,
            created_rows,
            created_cols,
            cell_changes: Vec::new(),
            meta_ops: Vec::new(),
        });
    }

    // Step 3: Materialize SortHunks (E15 folded blocks).
    let mut sort_inverse_rows: SortInverseMap = BTreeMap::new();
    for op in raw_sort_ops {
        if let Op::PermuteContent {
            sheet,
            ref rows,
            ref cols,
            ref row_perm,
        } = op
        {
            let hunk_id = next_hunk_id("sort");
            let sheet_name = draft_sheet_names
                .get(&sheet)
                .cloned()
                .unwrap_or_else(|| format!("Sheet#{}", sheet.0));
            let range_str = format_id_rect(draft.sheet(sheet), rows, cols);
            let mut has_conflict = false;
            let mut conflict_reason = None;
            let mut sort_target_deleted = false;
            if let Some(main_s) = main_head.live_sheet(sheet) {
                if rows
                    .iter()
                    .any(|r| main_s.row_axis().id_to_pos(r.0).is_none())
                    || cols.iter().any(|c| !main_s.col_axis().is_live(c.0))
                {
                    has_conflict = true;
                    sort_target_deleted = true;
                    conflict_reason = Some(
                        "Main branch deleted rows or columns inside the sorted range".to_owned(),
                    );
                } else if let Some(base_s) = base.sheet(sheet) {
                    'outer: for &r in rows {
                        for &c in cols {
                            if logical_cell(base, sheet, r, c)
                                != logical_cell(main_head, sheet, r, c)
                                && !base_s.is_deleted()
                            {
                                has_conflict = true;
                                conflict_reason = Some(
                                    "Main branch modified cells inside the sorted range".to_owned(),
                                );
                                break 'outer;
                            }
                        }
                    }
                }
            } else if base.live_sheet(sheet).is_some() {
                has_conflict = true;
                sort_target_deleted = true;
                conflict_reason = Some("Target worksheet was deleted on main".to_owned());
            }
            // Build inverse mapping: destination RowId -> source RowId before sort.
            let mut inv_map = BTreeMap::new();
            for (dst_idx, &src_idx) in row_perm.iter().enumerate() {
                if let (Some(&dst_r), Some(&src_r)) =
                    (rows.get(dst_idx), rows.get(src_idx as usize))
                {
                    inv_map.insert(dst_r, src_r);
                }
            }
            sort_inverse_rows.insert(
                hunk_id.clone(),
                (sheet, cols.iter().copied().collect(), inv_map),
            );
            let label = format!("Sort {sheet_name}!{range_str} ({} rows)", rows.len());
            public_hunks.push(DiffHunk {
                id: hunk_id.clone(),
                kind: DiffHunkKind::Sort,
                sheet_id: sheet.0.to_string(),
                sheet_name,
                label: label.clone(),
                default_accepted: !sort_target_deleted && !has_conflict,
                has_conflict,
                conflict_reason,
                anchor_cell: range_str.split(':').next().map(str::to_owned),
                cells: Vec::new(),
                structural_op_summary: None,
                sort_summary: Some(SortHunkSummary {
                    range: range_str,
                    row_count: rows.len() as u32,
                    col_count: cols.len() as u32,
                }),
            });
            planned_hunks.push(PlannedHunk {
                hunk_id,
                label,
                kind: DiffHunkKind::Sort,
                structural_ops: Vec::new(),
                sort_op: Some(op),
                created_sheets: BTreeSet::new(),
                created_rows: BTreeSet::new(),
                created_cols: BTreeSet::new(),
                cell_changes: Vec::new(),
                meta_ops: Vec::new(),
            });
        }
    }

    // Step 4: Compare cells across all live worksheets in draft.
    // Newly created sheet/row/col edits attach atomically to their owning StructuralHunk.
    // Pre-existing cells are grouped into CellBlockHunks (or MetaHunks for pure format/style).
    let mut cascaded_cells: Vec<CascadedCell> = Vec::new();
    let mut existing_changes_by_sheet: BTreeMap<SheetId, Vec<ExistingCellChange>> = BTreeMap::new();

    for draft_sheet in draft.live_sheets() {
        let sid = draft_sheet.id();
        let sheet_name = draft_sheet.name().to_owned();

        let mut candidate_keys = collect_sheet_populated_keys(draft, draft_sheet);
        if let Some(base_s) = base_after_sorts.sheet(sid) {
            candidate_keys.extend(collect_sheet_populated_keys(&base_after_sorts, base_s));
        }
        if let Some(fmt_keys) = format_touched_keys.get(&sid) {
            candidate_keys.extend(fmt_keys.iter().copied());
        }

        for (row, col) in candidate_keys {
            let Some(r_pos) = draft_sheet.row_axis().id_to_pos(row.0) else {
                continue;
            };
            let Some(c_pos) = draft_sheet.col_axis().id_to_pos(col.0) else {
                continue;
            };

            let draft_lc = logical_cell(draft, sid, row, col);
            let owner_idx = new_sheet_owner
                .get(&sid)
                .or_else(|| new_row_owner.get(&(sid, row)))
                .or_else(|| new_col_owner.get(&(sid, col)))
                .copied();

            if let Some(struct_idx) = owner_idx {
                if draft_lc.content == LogicalContent::Empty
                    && draft_lc.format.is_none()
                    && draft_lc.style.is_empty()
                {
                    continue;
                }
                let address = format_a1(r_pos, c_pos);
                let cell_key = format_cell_key(sid, row, col);
                let (draft_val, draft_form) =
                    format_display_and_formula(&draft_eval, sid, row, col);
                let entry = CellDiffEntry {
                    cell_key: cell_key.clone(),
                    sheet_name: sheet_name.clone(),
                    address: address.clone(),
                    row_pos: r_pos,
                    col_pos: c_pos,
                    base_value: String::new(),
                    base_formula: None,
                    draft_value: draft_val,
                    draft_formula: draft_form.clone(),
                    main_value: String::new(),
                    main_formula: None,
                    conflict: false,
                    default_resolution: ConflictResolution::TakeDraft,
                };
                if let Some(pub_h) = public_hunks.get_mut(struct_idx) {
                    if pub_h.anchor_cell.is_none() {
                        pub_h.anchor_cell = Some(address.clone());
                    }
                    pub_h.cells.push(entry);
                }
                if let Some(plan_h) = planned_hunks.get_mut(struct_idx) {
                    plan_h.cell_changes.push(PlannedCellChange {
                        cell_key,
                        sheet: sid,
                        row,
                        col,
                        address,
                        sheet_name: sheet_name.clone(),
                        formula_text: draft_form,
                        input: draft_lc.content.to_cell_input(),
                        format: draft_lc.format.map_or(FormatPatch::Keep, FormatPatch::Set),
                        style: (!draft_lc.style.is_empty()).then_some(draft_lc.style),
                        conflict: false,
                        deleted_on_main: false,
                    });
                }
                continue;
            }

            let base_lc = logical_cell(&base_after_sorts, sid, row, col);
            let main_target_live = main_head.live_sheet(sid).is_some_and(|ms| {
                ms.row_axis().id_to_pos(row.0).is_some() && ms.col_axis().is_live(col.0)
            });
            let main_lc = logical_cell(&main_after_sorts, sid, row, col);

            if draft_lc == base_lc {
                // E21 check: logical content & format unchanged. Only report non-volatile cascade
                // if this is a formula whose evaluated result genuinely shifted from upstream data.
                if let LogicalContent::Formula(ref f) = draft_lc.content
                    && !formula_is_volatile(&f.root)
                {
                    let (b_val, _) =
                        format_display_and_formula(&base_after_sorts_eval, sid, row, col);
                    let (d_val, d_form) = format_display_and_formula(&draft_eval, sid, row, col);
                    if b_val != d_val {
                        cascaded_cells.push(CascadedCell {
                            sheet_name: sheet_name.clone(),
                            address: format_a1(r_pos, c_pos),
                            base_value: b_val,
                            draft_value: d_val,
                            formula: d_form.unwrap_or_default(),
                        });
                    }
                }
                continue;
            }

            if main_target_live && main_lc == draft_lc {
                // Converged identically on both branches.
                continue;
            }

            let deleted_on_main = !main_target_live;
            let is_conflict = deleted_on_main || main_lc != base_lc;
            let pure_format = !deleted_on_main && draft_lc.content == base_lc.content;
            let address = format_a1(r_pos, c_pos);
            let cell_key = format_cell_key(sid, row, col);
            let (base_val, base_form) =
                format_display_and_formula(&base_after_sorts_eval, sid, row, col);
            let (draft_val, draft_form) = format_display_and_formula(&draft_eval, sid, row, col);
            let (main_val, main_form) = if deleted_on_main {
                ("<deleted on main>".to_owned(), None)
            } else {
                format_display_and_formula(&main_after_sorts_eval, sid, row, col)
            };

            let entry = CellDiffEntry {
                cell_key: cell_key.clone(),
                sheet_name: sheet_name.clone(),
                address: address.clone(),
                row_pos: r_pos,
                col_pos: c_pos,
                base_value: base_val,
                base_formula: base_form,
                draft_value: draft_val,
                draft_formula: draft_form.clone(),
                main_value: main_val,
                main_formula: main_form,
                conflict: is_conflict,
                default_resolution: if is_conflict {
                    ConflictResolution::KeepMain
                } else {
                    ConflictResolution::TakeDraft
                },
            };
            let format_patch = if draft_lc.format != base_lc.format {
                match draft_lc.format {
                    Some(fmt) => FormatPatch::Set(fmt),
                    None => FormatPatch::Clear,
                }
            } else {
                FormatPatch::Keep
            };
            let style_patch = if draft_lc.style != base_lc.style {
                Some(draft_lc.style)
            } else {
                None
            };
            let change = PlannedCellChange {
                cell_key,
                sheet: sid,
                row,
                col,
                address,
                sheet_name: sheet_name.clone(),
                formula_text: draft_form,
                input: draft_lc.content.to_cell_input(),
                format: format_patch,
                style: style_patch,
                conflict: is_conflict,
                deleted_on_main,
            };
            existing_changes_by_sheet.entry(sid).or_default().push((
                r_pos,
                c_pos,
                entry,
                change,
                pure_format,
            ));
        }
    }

    // Group non-conflicting existing cell changes into contiguous blocks; split conflict cells cleanly.
    for (sid, mut items) in existing_changes_by_sheet {
        items.sort_by_key(|&(r, c, _, _, _)| (r, c));
        let sheet_name = draft_sheet_names
            .get(&sid)
            .cloned()
            .unwrap_or_else(|| format!("Sheet#{}", sid.0));

        let mut conflict_items = Vec::new();
        let mut format_items = Vec::new();
        let mut content_items = Vec::new();
        for item in items {
            if item.2.conflict {
                conflict_items.push(item);
            } else if item.4 {
                format_items.push(item);
            } else {
                content_items.push(item);
            }
        }

        // Conflict cells each become an explicit high-visibility hunk (or grouped by conflict) so user chooses KeepMain/TakeDraft.
        for (_, _, entry, change, _) in conflict_items {
            let hunk_id = next_hunk_id("conflict");
            let label = format!("Conflict at {sheet_name}!{}", entry.address);
            let anchor = Some(entry.address.clone());
            let reason = if change.deleted_on_main {
                "Main branch deleted this cell's row, column, or worksheet since the fork point"
                    .to_owned()
            } else {
                "Both main and AI draft modified this cell since the fork point".to_owned()
            };
            public_hunks.push(DiffHunk {
                id: hunk_id.clone(),
                kind: DiffHunkKind::CellBlock,
                sheet_id: sid.0.to_string(),
                sheet_name: sheet_name.clone(),
                label: label.clone(),
                default_accepted: !change.deleted_on_main,
                has_conflict: true,
                conflict_reason: Some(reason),
                anchor_cell: anchor,
                cells: vec![entry],
                structural_op_summary: None,
                sort_summary: None,
            });
            planned_hunks.push(PlannedHunk {
                hunk_id,
                label,
                kind: DiffHunkKind::CellBlock,
                structural_ops: Vec::new(),
                sort_op: None,
                created_sheets: BTreeSet::new(),
                created_rows: BTreeSet::new(),
                created_cols: BTreeSet::new(),
                cell_changes: vec![change],
                meta_ops: Vec::new(),
            });
        }

        // Cluster content edits into contiguous blocks (adjacent within 1 row or 1 col).
        for cluster in cluster_adjacent_cells(content_items) {
            let hunk_id = next_hunk_id("cells");
            let first_addr = cluster
                .first()
                .map(|c| c.2.address.clone())
                .unwrap_or_default();
            let last_addr = cluster
                .last()
                .map(|c| c.2.address.clone())
                .unwrap_or_default();
            let span = if first_addr == last_addr {
                first_addr.clone()
            } else {
                format!("{first_addr}:{last_addr}")
            };
            let label = format!("Edit {sheet_name}!{span} ({} cells)", cluster.len());
            let cells: Vec<CellDiffEntry> = cluster.iter().map(|c| c.2.clone()).collect();
            let changes: Vec<PlannedCellChange> = cluster.into_iter().map(|c| c.3).collect();
            public_hunks.push(DiffHunk {
                id: hunk_id.clone(),
                kind: DiffHunkKind::CellBlock,
                sheet_id: sid.0.to_string(),
                sheet_name: sheet_name.clone(),
                label: label.clone(),
                default_accepted: true,
                has_conflict: false,
                conflict_reason: None,
                anchor_cell: Some(first_addr),
                cells,
                structural_op_summary: None,
                sort_summary: None,
            });
            planned_hunks.push(PlannedHunk {
                hunk_id,
                label,
                kind: DiffHunkKind::CellBlock,
                structural_ops: Vec::new(),
                sort_op: None,
                created_sheets: BTreeSet::new(),
                created_rows: BTreeSet::new(),
                created_cols: BTreeSet::new(),
                cell_changes: changes,
                meta_ops: Vec::new(),
            });
        }

        // Pure format/style changes -> MetaHunk per sheet.
        if !format_items.is_empty() {
            let hunk_id = next_hunk_id("meta");
            let first_addr = format_items
                .first()
                .map(|c| c.2.address.clone())
                .unwrap_or_default();
            let label = format!("Format {sheet_name} ({} cells)", format_items.len());
            let mut meta_ops = Vec::new();
            let mut cells = Vec::new();
            for (_, _, entry, change, _) in format_items {
                cells.push(entry);
                let style_patch = change.style.unwrap_or_default();
                let has_fmt_or_style =
                    change.format != FormatPatch::Keep || !style_patch.is_empty();
                if has_fmt_or_style
                    && let (Some(rr), Some(cr)) =
                        (IdRun::new(change.row.0, 1), IdRun::new(change.col.0, 1))
                {
                    meta_ops.push(Op::SetFormat {
                        sheet: sid,
                        target: FormatTarget::Cells {
                            rows: vec![rr],
                            cols: vec![cr],
                        },
                        number_format: change.format,
                        style: style_patch,
                    });
                }
            }
            if !meta_ops.is_empty() {
                public_hunks.push(DiffHunk {
                    id: hunk_id.clone(),
                    kind: DiffHunkKind::Meta,
                    sheet_id: sid.0.to_string(),
                    sheet_name: sheet_name.clone(),
                    label: label.clone(),
                    default_accepted: true,
                    has_conflict: false,
                    conflict_reason: None,
                    anchor_cell: Some(first_addr),
                    cells,
                    structural_op_summary: Some(label.clone()),
                    sort_summary: None,
                });
                planned_hunks.push(PlannedHunk {
                    hunk_id,
                    label,
                    kind: DiffHunkKind::Meta,
                    structural_ops: Vec::new(),
                    sort_op: None,
                    created_sheets: BTreeSet::new(),
                    created_rows: BTreeSet::new(),
                    created_cols: BTreeSet::new(),
                    cell_changes: Vec::new(),
                    meta_ops,
                });
            }
        }
    }

    // Step 5: Named range metadata changes -> MetaHunks.
    let mut all_name_keys: BTreeSet<NamedRangeKey> = BTreeSet::new();
    all_name_keys.extend(base.named_ranges().keys().cloned());
    all_name_keys.extend(draft.named_ranges().keys().cloned());
    for key in all_name_keys {
        let b_def = base.named_ranges().get(&key);
        let d_def = draft.named_ranges().get(&key);
        let m_def = main_head.named_ranges().get(&key);
        if b_def == d_def || m_def == d_def {
            continue;
        }
        let has_conflict = m_def != b_def;
        let hunk_id = next_hunk_id("meta");
        let scope_sheet = key.scope.unwrap_or(SheetId(0));
        let sheet_name = key
            .scope
            .and_then(|s| draft_sheet_names.get(&s).cloned())
            .unwrap_or_else(|| "Workbook".to_owned());
        let (label, meta_op) = match d_def {
            Some(def) => {
                let unparsed = def.formula.unparse(draft, scope_sheet);
                named_range_formula_texts.insert(hunk_id.clone(), unparsed.clone());
                let rhs = unparsed.strip_prefix('=').unwrap_or(&unparsed);
                (
                    format!("Define named range {} = {rhs}", def.name),
                    Op::DefineNamedRange {
                        scope: key.scope,
                        name: def.name.clone(),
                        formula: def.formula.clone(),
                        comment: def.comment.clone(),
                        hidden: def.hidden,
                    },
                )
            }
            None => (
                format!("Delete named range {}", key.canonical_name),
                Op::DeleteNamedRange {
                    scope: key.scope,
                    name: b_def
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| key.canonical_name.clone()),
                },
            ),
        };
        public_hunks.push(DiffHunk {
            id: hunk_id.clone(),
            kind: DiffHunkKind::Meta,
            sheet_id: scope_sheet.0.to_string(),
            sheet_name,
            label: label.clone(),
            default_accepted: !has_conflict,
            has_conflict,
            conflict_reason: has_conflict
                .then(|| "Named range modified concurrently on main".to_owned()),
            anchor_cell: None,
            cells: Vec::new(),
            structural_op_summary: Some(label.clone()),
            sort_summary: None,
        });
        planned_hunks.push(PlannedHunk {
            hunk_id,
            label,
            kind: DiffHunkKind::Meta,
            structural_ops: Vec::new(),
            sort_op: None,
            created_sheets: BTreeSet::new(),
            created_rows: BTreeSet::new(),
            created_cols: BTreeSet::new(),
            cell_changes: Vec::new(),
            meta_ops: vec![meta_op],
        });
    }

    // Build summary stats.
    let mut summary = DiffSummary {
        total_hunks: public_hunks.len() as u32,
        cascaded_cells: cascaded_cells.len() as u32,
        ..DiffSummary::default()
    };
    for h in &public_hunks {
        match h.kind {
            DiffHunkKind::Structural => summary.structural_hunks += 1,
            DiffHunkKind::Sort => summary.sort_hunks += 1,
            DiffHunkKind::CellBlock => summary.cell_block_hunks += 1,
            DiffHunkKind::Meta => summary.meta_hunks += 1,
        }
        summary.changed_cells += h.cells.len() as u32;
        summary.conflict_cells += h.cells.iter().filter(|c| c.conflict).count() as u32;
    }

    BranchDiffPlan {
        diff: BranchDiff {
            fork_seq: fork_seq.to_string(),
            main_head_seq: main_head_seq.to_string(),
            hunks: public_hunks,
            cascaded_cells,
            summary,
        },
        hunks: planned_hunks,
        draft_sheet_names,
        draft_row_pos,
        draft_col_pos,
        named_range_formula_texts,
        sort_inverse_rows,
    }
}

/// Performs static reference closure validation over the user's selected hunks.
/// Checks every accepted cell formula, accepted named range,
/// and accepted structural/sort/meta operation against every sheet/row/column
/// introduced by rejected structural hunks.
pub fn check_reference_closure(
    plan: &BranchDiffPlan,
    selection: &MergeSelection,
) -> Vec<ClosureIssue> {
    let accepted_set: BTreeSet<&str> = selection
        .accepted_hunks
        .iter()
        .map(String::as_str)
        .collect();
    let rejected = collect_rejected_new_ids(plan, &accepted_set);
    if rejected.is_empty() {
        return Vec::new();
    }

    let mut issues = Vec::new();
    for h in &plan.hunks {
        if !accepted_set.contains(h.hunk_id.as_str()) {
            continue;
        }
        // Check structural/sort/meta operation targets against rejected created sheets/rows/cols.
        for op in &h.structural_ops {
            check_op_target_closure(op, &h.label, plan, &rejected, &mut issues);
        }
        if let Some(ref sop) = h.sort_op {
            check_op_target_closure(sop, &h.label, plan, &rejected, &mut issues);
        }
        for cell in &h.cell_changes {
            if cell.conflict
                && selection.conflict_choices.get(&cell.cell_key)
                    != Some(&ConflictResolution::TakeDraft)
            {
                continue;
            }
            if let CellInput::Formula { ref formula, .. } = cell.input {
                let source = format!("{}!{}", cell.sheet_name, cell.address);
                let formula_text = cell
                    .formula_text
                    .clone()
                    .unwrap_or_else(|| format_bound_summary(&formula.root));
                walk_expr_closure(
                    &formula.root,
                    cell.sheet,
                    &source,
                    &formula_text,
                    &rejected,
                    plan,
                    &mut issues,
                );
            }
        }
        for op in &h.meta_ops {
            check_op_target_closure(op, &h.label, plan, &rejected, &mut issues);
            if let Op::DefineNamedRange {
                scope,
                name,
                formula,
                ..
            } = op
            {
                let host = scope.unwrap_or(SheetId(0));
                let source = format!("NamedRange:{name}");
                let formula_text = plan
                    .named_range_formula_texts
                    .get(&h.hunk_id)
                    .cloned()
                    .unwrap_or_else(|| format_bound_summary(&formula.root));
                walk_expr_closure(
                    &formula.root,
                    host,
                    &source,
                    &formula_text,
                    &rejected,
                    plan,
                    &mut issues,
                );
            }
        }
    }
    issues
}

/// Evaluates a merge preview (unresolved conflicts + reference closure + op count).
pub fn preview_merge(plan: &BranchDiffPlan, selection: &MergeSelection) -> MergePreviewResponse {
    let unresolved = collect_unresolved_conflicts(plan, selection);
    let closure_issues = check_reference_closure(plan, selection);
    let can_merge =
        unresolved.is_empty() && (closure_issues.is_empty() || selection.allow_ref_errors);
    let accepted_set: BTreeSet<&str> = selection
        .accepted_hunks
        .iter()
        .map(String::as_str)
        .collect();
    let estimated_ops: u32 = plan
        .hunks
        .iter()
        .filter(|h| accepted_set.contains(h.hunk_id.as_str()))
        .map(|h| {
            let mut n =
                h.structural_ops.len() + h.meta_ops.len() + usize::from(h.sort_op.is_some());
            if !h.cell_changes.is_empty() {
                n += h.cell_changes.len().div_ceil(MERGE_SET_CELLS_CHUNK);
            }
            n as u32
        })
        .sum();
    MergePreviewResponse {
        can_merge,
        main_head_seq: plan.diff.main_head_seq.clone(),
        closure_issues,
        unresolved_conflicts: unresolved,
        estimated_ops,
    }
}

/// Synthesizes sharded, dependency-ordered, `RemapState`-remapped operations ready
/// to commit onto `main` under `(epoch, merge_replica)`.
pub fn plan_merge_ops(
    plan: &BranchDiffPlan,
    selection: &MergeSelection,
    main_head: &Workbook<BoundFormula>,
    epoch: u8,
    merge_replica: u32,
    start_id_counter: u32,
) -> Result<Vec<Vec<Op<BoundFormula>>>, MergePlanError> {
    let unresolved = collect_unresolved_conflicts(plan, selection);
    if !unresolved.is_empty() {
        return Err(MergePlanError::UnresolvedConflicts(unresolved));
    }
    let closure_issues = check_reference_closure(plan, selection);
    if !closure_issues.is_empty() && !selection.allow_ref_errors {
        return Err(MergePlanError::ClosureBlocked(closure_issues));
    }

    let accepted_set: BTreeSet<&str> = selection
        .accepted_hunks
        .iter()
        .map(String::as_str)
        .collect();
    let rejected = collect_rejected_new_ids(plan, &accepted_set);

    // Collect rejected sort inverse mappings so post-sort cell edits still land on
    // the right pre-sort row record if the user declined the SortHunk.
    let mut rejected_sorts: Vec<&(SheetId, BTreeSet<ColId>, BTreeMap<RowId, RowId>)> = Vec::new();
    for (hunk_id, info) in &plan.sort_inverse_rows {
        if !accepted_set.contains(hunk_id.as_str()) {
            rejected_sorts.push(info);
        }
    }

    // Topological phase buckets:
    // 1. AddSheet
    // 2. InsertRows / InsertCols
    // 3. RenameSheet / MoveSheet / MoveRows / MoveCols
    // 4. PermuteContent (SortHunks)
    // 5. SetCells (sharded) + SetFormat for cell styles
    // 6. Meta ops (SetFormat / DefineNamedRange / DeleteNamedRange / Resize / Freeze)
    // 7. DeleteRows / DeleteCols / DeleteSheet
    let mut add_sheets = Vec::new();
    let mut inserts = Vec::new();
    let mut moves_renames = Vec::new();
    let mut sorts = Vec::new();
    let mut named_range_defs = Vec::new();
    let mut cell_edits_by_sheet: BTreeMap<SheetId, Vec<CellEdit<BoundFormula>>> = BTreeMap::new();
    let mut style_ops = Vec::new();
    let mut meta_ops = Vec::new();
    let mut deletes = Vec::new();

    for h in &plan.hunks {
        if !accepted_set.contains(h.hunk_id.as_str()) {
            continue;
        }
        for op in &h.structural_ops {
            match op {
                Op::AddSheet { .. } => add_sheets.push(op.clone()),
                Op::InsertRows { .. } | Op::InsertCols { .. } => inserts.push(op.clone()),
                Op::RenameSheet { .. }
                | Op::MoveSheet { .. }
                | Op::MoveRows { .. }
                | Op::MoveCols { .. } => moves_renames.push(op.clone()),
                Op::DeleteRows { .. } | Op::DeleteCols { .. } | Op::DeleteSheet { .. } => {
                    deletes.push(op.clone());
                }
                _ => {}
            }
        }
        if let Some(ref sop) = h.sort_op {
            sorts.push(sop.clone());
        }
        for cell in &h.cell_changes {
            if cell.conflict
                && selection.conflict_choices.get(&cell.cell_key)
                    != Some(&ConflictResolution::TakeDraft)
            {
                continue;
            }
            if cell.deleted_on_main {
                return Err(MergePlanError::RemapFailed(format!(
                    "cannot apply draft edit at {}!{}: target row, column, or worksheet was deleted on main",
                    cell.sheet_name, cell.address
                )));
            }
            let mut target_row = cell.row;
            if h.kind == DiffHunkKind::CellBlock {
                // Reverse-map row across any rejected sorts covering `(sheet, col)`.
                for (sort_sheet, sort_cols, inv_map) in rejected_sorts.iter().rev() {
                    if *sort_sheet == cell.sheet
                        && sort_cols.contains(&cell.col)
                        && let Some(&orig_r) = inv_map.get(&target_row)
                    {
                        target_row = orig_r;
                    }
                }
            }
            let mut input = cell.input.clone();
            if selection.allow_ref_errors
                && !rejected.is_empty()
                && let CellInput::Formula {
                    ref mut formula, ..
                } = input
            {
                sanitize_rejected_expr(&mut formula.root, cell.sheet, &rejected, plan);
            }
            cell_edits_by_sheet
                .entry(cell.sheet)
                .or_default()
                .push(CellEdit {
                    row: target_row,
                    col: cell.col,
                    input,
                    format: cell.format,
                });
            if let Some(style) = cell.style
                && !style.is_empty()
                && let (Some(rr), Some(cr)) =
                    (IdRun::new(target_row.0, 1), IdRun::new(cell.col.0, 1))
            {
                style_ops.push(Op::SetFormat {
                    sheet: cell.sheet,
                    target: FormatTarget::Cells {
                        rows: vec![rr],
                        cols: vec![cr],
                    },
                    number_format: FormatPatch::Keep,
                    style,
                });
            }
        }
        for op in &h.meta_ops {
            let mut cloned = op.clone();
            if selection.allow_ref_errors
                && !rejected.is_empty()
                && let Op::DefineNamedRange {
                    scope,
                    ref mut formula,
                    ..
                } = cloned
            {
                sanitize_rejected_expr(
                    &mut formula.root,
                    scope.unwrap_or(SheetId(0)),
                    &rejected,
                    plan,
                );
            }
            if matches!(cloned, Op::DefineNamedRange { .. }) {
                named_range_defs.push(cloned);
            } else {
                meta_ops.push(cloned);
            }
        }
    }

    let mut ordered: Vec<Op<BoundFormula>> = Vec::new();
    ordered.extend(add_sheets);
    ordered.extend(inserts);
    ordered.extend(moves_renames);
    ordered.extend(sorts);
    ordered.extend(named_range_defs);
    for (sheet, edits) in cell_edits_by_sheet {
        let mut chunk = Vec::new();
        let mut chunk_bytes = 0_usize;
        for edit in edits {
            let edit_bytes = postcard::to_allocvec(&edit).map_or(256, |v| v.len());
            if !chunk.is_empty()
                && (chunk.len() >= MERGE_SET_CELLS_CHUNK
                    || chunk_bytes.saturating_add(edit_bytes) > 900_000)
            {
                ordered.push(Op::SetCells {
                    sheet,
                    edits: std::mem::take(&mut chunk),
                });
                chunk_bytes = 0;
            }
            chunk_bytes = chunk_bytes.saturating_add(edit_bytes);
            chunk.push(edit);
        }
        if !chunk.is_empty() {
            ordered.push(Op::SetCells {
                sheet,
                edits: chunk,
            });
        }
    }
    ordered.extend(style_ops);
    ordered.extend(meta_ops);
    ordered.extend(deletes);

    if ordered.is_empty() {
        return Ok(Vec::new());
    }

    // Remap all draft-allocated IDs to fresh IDs belonging to `(epoch, merge_replica)`.
    let mut id_alloc =
        IdAllocator::new(epoch, merge_replica, start_id_counter.max(1)).ok_or_else(|| {
            MergePlanError::RemapFailed("cannot initialize merge IdAllocator".to_owned())
        })?;
    let mut sheet_counter = 1_u8;
    let mut remap = RemapState::new(
        epoch,
        merge_replica,
        &mut id_alloc,
        &mut sheet_counter,
        main_head,
    );

    let mut remapped = Vec::with_capacity(ordered.len());
    for op in ordered {
        let mapped = remap.remap_op(op).ok_or_else(|| {
            MergePlanError::RemapFailed("exhausted ID space during merge remap".to_owned())
        })?;
        remapped.push(mapped);
    }

    // Shard into transaction batches staying safely below 1.2 MiB per wire transaction.
    let mut batches: Vec<Vec<Op<BoundFormula>>> = Vec::new();
    let mut current_batch: Vec<Op<BoundFormula>> = Vec::new();
    let mut current_est_bytes: usize = 0;

    for op in remapped {
        let op_bytes = postcard::to_allocvec(&op).map_or(1024, |v| v.len());
        if !current_batch.is_empty() && current_est_bytes.saturating_add(op_bytes) > 1_200_000 {
            batches.push(std::mem::take(&mut current_batch));
            current_est_bytes = 0;
        }
        current_est_bytes = current_est_bytes.saturating_add(op_bytes);
        current_batch.push(op);
    }
    if !current_batch.is_empty() {
        batches.push(current_batch);
    }
    Ok(batches)
}

#[derive(Default)]
struct RejectedIds {
    sheets: BTreeMap<SheetId, (String, String)>,
    rows: BTreeMap<(SheetId, RowId), (String, String)>,
    cols: BTreeMap<(SheetId, ColId), (String, String)>,
}

impl RejectedIds {
    fn is_empty(&self) -> bool {
        self.sheets.is_empty() && self.rows.is_empty() && self.cols.is_empty()
    }
}

fn collect_rejected_new_ids(plan: &BranchDiffPlan, accepted: &BTreeSet<&str>) -> RejectedIds {
    let mut rej = RejectedIds::default();
    for h in &plan.hunks {
        if h.kind != DiffHunkKind::Structural || accepted.contains(h.hunk_id.as_str()) {
            continue;
        }
        let info = (h.hunk_id.clone(), h.label.clone());
        for &s in &h.created_sheets {
            rej.sheets.insert(s, info.clone());
        }
        for &r in &h.created_rows {
            rej.rows.insert(r, info.clone());
        }
        for &c in &h.created_cols {
            rej.cols.insert(c, info.clone());
        }
    }
    rej
}

fn collect_unresolved_conflicts(plan: &BranchDiffPlan, selection: &MergeSelection) -> Vec<String> {
    let accepted: BTreeSet<&str> = selection
        .accepted_hunks
        .iter()
        .map(String::as_str)
        .collect();
    let mut unresolved = Vec::new();
    for h in &plan.hunks {
        if !accepted.contains(h.hunk_id.as_str()) {
            continue;
        }
        for cell in &h.cell_changes {
            if cell.conflict && !selection.conflict_choices.contains_key(&cell.cell_key) {
                unresolved.push(cell.cell_key.clone());
            }
        }
    }
    unresolved
}

fn format_row_label(plan: &BranchDiffPlan, sheet: SheetId, sname: &str, row: RowId) -> String {
    if let Some(&pos) = plan.draft_row_pos.get(&(sheet, row)) {
        format!("{sname} row {}", pos + 1)
    } else {
        format!("{sname} row#{}", row.0)
    }
}

fn format_col_label(plan: &BranchDiffPlan, sheet: SheetId, sname: &str, col: ColId) -> String {
    if let Some(&pos) = plan.draft_col_pos.get(&(sheet, col)) {
        let mut letters = String::new();
        write_col_letters(pos, &mut letters);
        format!("{sname} col {letters}")
    } else {
        format!("{sname} col#{}", col.0)
    }
}

fn push_rejected_sheet_issue(
    sid: SheetId,
    source_label: &str,
    plan: &BranchDiffPlan,
    rejected: &RejectedIds,
    issues: &mut Vec<ClosureIssue>,
) {
    if let Some((hid, hlabel)) = rejected.sheets.get(&sid) {
        let sname = plan
            .draft_sheet_names
            .get(&sid)
            .cloned()
            .unwrap_or_else(|| format!("Sheet#{}", sid.0));
        issues.push(ClosureIssue {
            source_cell: source_label.to_owned(),
            formula_text: source_label.to_owned(),
            target_hunk_id: hid.clone(),
            target_hunk_label: hlabel.clone(),
            ref_display: format!("sheet '{sname}'"),
        });
    }
}

fn push_rejected_row_issue(
    sid: SheetId,
    row: RowId,
    source_label: &str,
    plan: &BranchDiffPlan,
    rejected: &RejectedIds,
    issues: &mut Vec<ClosureIssue>,
) {
    if let Some((hid, hlabel)) = rejected.rows.get(&(sid, row)) {
        let sname = plan
            .draft_sheet_names
            .get(&sid)
            .cloned()
            .unwrap_or_else(|| format!("Sheet#{}", sid.0));
        issues.push(ClosureIssue {
            source_cell: source_label.to_owned(),
            formula_text: source_label.to_owned(),
            target_hunk_id: hid.clone(),
            target_hunk_label: hlabel.clone(),
            ref_display: format_row_label(plan, sid, &sname, row),
        });
    }
}

fn push_rejected_col_issue(
    sid: SheetId,
    col: ColId,
    source_label: &str,
    plan: &BranchDiffPlan,
    rejected: &RejectedIds,
    issues: &mut Vec<ClosureIssue>,
) {
    if let Some((hid, hlabel)) = rejected.cols.get(&(sid, col)) {
        let sname = plan
            .draft_sheet_names
            .get(&sid)
            .cloned()
            .unwrap_or_else(|| format!("Sheet#{}", sid.0));
        issues.push(ClosureIssue {
            source_cell: source_label.to_owned(),
            formula_text: source_label.to_owned(),
            target_hunk_id: hid.clone(),
            target_hunk_label: hlabel.clone(),
            ref_display: format_col_label(plan, sid, &sname, col),
        });
    }
}

fn check_op_target_closure(
    op: &Op<BoundFormula>,
    source_label: &str,
    plan: &BranchDiffPlan,
    rejected: &RejectedIds,
    issues: &mut Vec<ClosureIssue>,
) {
    match op {
        Op::AddSheet {
            after: Some(prev), ..
        } => push_rejected_sheet_issue(*prev, source_label, plan, rejected, issues),
        Op::DeleteSheet { sheet } | Op::RenameSheet { sheet, .. } => {
            push_rejected_sheet_issue(*sheet, source_label, plan, rejected, issues);
        }
        Op::MoveSheet { sheet, after } => {
            push_rejected_sheet_issue(*sheet, source_label, plan, rejected, issues);
            if let Some(prev) = after {
                push_rejected_sheet_issue(*prev, source_label, plan, rejected, issues);
            }
        }
        Op::InsertRows { sheet, after, .. } => {
            push_rejected_sheet_issue(*sheet, source_label, plan, rejected, issues);
            if let Some(anchor) = after {
                push_rejected_row_issue(*sheet, *anchor, source_label, plan, rejected, issues);
            }
        }
        Op::InsertCols { sheet, after, .. } => {
            push_rejected_sheet_issue(*sheet, source_label, plan, rejected, issues);
            if let Some(anchor) = after {
                push_rejected_col_issue(*sheet, *anchor, source_label, plan, rejected, issues);
            }
        }
        Op::DeleteRows { sheet, ids } => {
            push_rejected_sheet_issue(*sheet, source_label, plan, rejected, issues);
            for raw in ids.iter().flat_map(|r| r.iter()) {
                push_rejected_row_issue(*sheet, RowId(raw), source_label, plan, rejected, issues);
            }
        }
        Op::DeleteCols { sheet, ids } => {
            push_rejected_sheet_issue(*sheet, source_label, plan, rejected, issues);
            for raw in ids.iter().flat_map(|c| c.iter()) {
                push_rejected_col_issue(*sheet, ColId(raw), source_label, plan, rejected, issues);
            }
        }
        Op::MoveRows { sheet, ids, after } => {
            push_rejected_sheet_issue(*sheet, source_label, plan, rejected, issues);
            for raw in ids.iter().flat_map(|r| r.iter()) {
                push_rejected_row_issue(*sheet, RowId(raw), source_label, plan, rejected, issues);
            }
            if let Some(anchor) = after {
                push_rejected_row_issue(*sheet, *anchor, source_label, plan, rejected, issues);
            }
        }
        Op::MoveCols { sheet, ids, after } => {
            push_rejected_sheet_issue(*sheet, source_label, plan, rejected, issues);
            for raw in ids.iter().flat_map(|c| c.iter()) {
                push_rejected_col_issue(*sheet, ColId(raw), source_label, plan, rejected, issues);
            }
            if let Some(anchor) = after {
                push_rejected_col_issue(*sheet, *anchor, source_label, plan, rejected, issues);
            }
        }
        Op::PermuteContent {
            sheet, rows, cols, ..
        } => {
            push_rejected_sheet_issue(*sheet, source_label, plan, rejected, issues);
            for &r in rows {
                push_rejected_row_issue(*sheet, r, source_label, plan, rejected, issues);
            }
            for &c in cols {
                push_rejected_col_issue(*sheet, c, source_label, plan, rejected, issues);
            }
        }
        Op::SetFormat { sheet, target, .. } => {
            push_rejected_sheet_issue(*sheet, source_label, plan, rejected, issues);
            match target {
                FormatTarget::Cells { rows, cols } => {
                    for raw in rows.iter().flat_map(|r| r.iter()) {
                        push_rejected_row_issue(
                            *sheet,
                            RowId(raw),
                            source_label,
                            plan,
                            rejected,
                            issues,
                        );
                    }
                    for raw in cols.iter().flat_map(|c| c.iter()) {
                        push_rejected_col_issue(
                            *sheet,
                            ColId(raw),
                            source_label,
                            plan,
                            rejected,
                            issues,
                        );
                    }
                }
                FormatTarget::Rows(rows) => {
                    for raw in rows.iter().flat_map(|r| r.iter()) {
                        push_rejected_row_issue(
                            *sheet,
                            RowId(raw),
                            source_label,
                            plan,
                            rejected,
                            issues,
                        );
                    }
                }
                FormatTarget::Cols(cols) => {
                    for raw in cols.iter().flat_map(|c| c.iter()) {
                        push_rejected_col_issue(
                            *sheet,
                            ColId(raw),
                            source_label,
                            plan,
                            rejected,
                            issues,
                        );
                    }
                }
            }
        }
        Op::DefineNamedRange {
            scope: Some(sid), ..
        }
        | Op::DeleteNamedRange {
            scope: Some(sid), ..
        } => push_rejected_sheet_issue(*sid, source_label, plan, rejected, issues),
        _ => {}
    }
}

fn walk_expr_closure(
    expr: &BoundExpr,
    host_sheet: SheetId,
    source_cell: &str,
    formula_text: &str,
    rejected: &RejectedIds,
    plan: &BranchDiffPlan,
    issues: &mut Vec<ClosureIssue>,
) {
    match expr {
        BoundExpr::Reference(r) => {
            if let Some((hunk_id, hunk_label, ref_display)) =
                ref_hits_rejected(r, host_sheet, rejected, plan)
            {
                issues.push(ClosureIssue {
                    source_cell: source_cell.to_owned(),
                    formula_text: formula_text.to_owned(),
                    target_hunk_id: hunk_id,
                    target_hunk_label: hunk_label,
                    ref_display,
                });
            }
        }
        BoundExpr::ScopedName {
            sheet: Some(sid),
            name,
            ..
        } => {
            if let Some((hunk_id, hunk_label)) = rejected.sheets.get(sid) {
                let sname = plan
                    .draft_sheet_names
                    .get(sid)
                    .cloned()
                    .unwrap_or_else(|| format!("Sheet#{}", sid.0));
                issues.push(ClosureIssue {
                    source_cell: source_cell.to_owned(),
                    formula_text: formula_text.to_owned(),
                    target_hunk_id: hunk_id.clone(),
                    target_hunk_label: hunk_label.clone(),
                    ref_display: format!("{sname}!{name}"),
                });
            }
        }
        BoundExpr::Paren(inner) | BoundExpr::SpillRef(inner) => {
            walk_expr_closure(
                inner,
                host_sheet,
                source_cell,
                formula_text,
                rejected,
                plan,
                issues,
            );
        }
        BoundExpr::Unary { expr: inner, .. } => {
            walk_expr_closure(
                inner,
                host_sheet,
                source_cell,
                formula_text,
                rejected,
                plan,
                issues,
            );
        }
        BoundExpr::Binary { left, right, .. } => {
            walk_expr_closure(
                left,
                host_sheet,
                source_cell,
                formula_text,
                rejected,
                plan,
                issues,
            );
            walk_expr_closure(
                right,
                host_sheet,
                source_cell,
                formula_text,
                rejected,
                plan,
                issues,
            );
        }
        BoundExpr::Call { args, .. } => {
            for arg in args {
                walk_expr_closure(
                    arg,
                    host_sheet,
                    source_cell,
                    formula_text,
                    rejected,
                    plan,
                    issues,
                );
            }
        }
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { sheet: None, .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => {}
    }
}

fn ref_hits_rejected(
    r: &BoundRef,
    host_sheet: SheetId,
    rejected: &RejectedIds,
    plan: &BranchDiffPlan,
) -> Option<(String, String, String)> {
    let target_sheet = match r {
        BoundRef::Cell { sheet, .. }
        | BoundRef::Area { sheet, .. }
        | BoundRef::ColRange { sheet, .. }
        | BoundRef::RowRange { sheet, .. } => sheet.unwrap_or(host_sheet),
        BoundRef::Invalid { .. } => return None,
    };
    let sname = plan
        .draft_sheet_names
        .get(&target_sheet)
        .cloned()
        .unwrap_or_else(|| format!("Sheet#{}", target_sheet.0));

    if let Some((hid, hlabel)) = rejected.sheets.get(&target_sheet) {
        return Some((hid.clone(), hlabel.clone(), format!("sheet '{sname}'")));
    }

    match r {
        BoundRef::Cell { row, col, .. } => {
            if let Some((hid, hlabel)) = rejected.rows.get(&(target_sheet, *row)) {
                return Some((
                    hid.clone(),
                    hlabel.clone(),
                    format_row_label(plan, target_sheet, &sname, *row),
                ));
            }
            if let Some((hid, hlabel)) = rejected.cols.get(&(target_sheet, *col)) {
                return Some((
                    hid.clone(),
                    hlabel.clone(),
                    format_col_label(plan, target_sheet, &sname, *col),
                ));
            }
        }
        BoundRef::Area {
            first_row,
            first_col,
            last_row,
            last_col,
            ..
        } => {
            for row in [*first_row, *last_row] {
                if let Some((hid, hlabel)) = rejected.rows.get(&(target_sheet, row)) {
                    return Some((
                        hid.clone(),
                        hlabel.clone(),
                        format_row_label(plan, target_sheet, &sname, row),
                    ));
                }
            }
            for col in [*first_col, *last_col] {
                if let Some((hid, hlabel)) = rejected.cols.get(&(target_sheet, col)) {
                    return Some((
                        hid.clone(),
                        hlabel.clone(),
                        format_col_label(plan, target_sheet, &sname, col),
                    ));
                }
            }
        }
        BoundRef::ColRange {
            first_col,
            last_col,
            ..
        } => {
            for col in [*first_col, *last_col] {
                if let Some((hid, hlabel)) = rejected.cols.get(&(target_sheet, col)) {
                    return Some((
                        hid.clone(),
                        hlabel.clone(),
                        format_col_label(plan, target_sheet, &sname, col),
                    ));
                }
            }
        }
        BoundRef::RowRange {
            first_row,
            last_row,
            ..
        } => {
            for row in [*first_row, *last_row] {
                if let Some((hid, hlabel)) = rejected.rows.get(&(target_sheet, row)) {
                    return Some((
                        hid.clone(),
                        hlabel.clone(),
                        format_row_label(plan, target_sheet, &sname, row),
                    ));
                }
            }
        }
        BoundRef::Invalid { .. } => {}
    }
    None
}

fn sanitize_rejected_expr(
    expr: &mut BoundExpr,
    host_sheet: SheetId,
    rejected: &RejectedIds,
    plan: &BranchDiffPlan,
) {
    match expr {
        BoundExpr::Reference(r) => {
            if ref_hits_rejected(r, host_sheet, rejected, plan).is_some() {
                let sheet_name = match r {
                    BoundRef::Cell { sheet, .. }
                    | BoundRef::Area { sheet, .. }
                    | BoundRef::ColRange { sheet, .. }
                    | BoundRef::RowRange { sheet, .. } => {
                        sheet.and_then(|sid| plan.draft_sheet_names.get(&sid).cloned())
                    }
                    BoundRef::Invalid { sheet_name } => sheet_name.clone(),
                };
                *r = BoundRef::Invalid { sheet_name };
            }
        }
        BoundExpr::ScopedName {
            sheet,
            missing_sheet,
            ..
        } => {
            if let Some(sid) = *sheet
                && rejected.sheets.contains_key(&sid)
            {
                *missing_sheet = plan.draft_sheet_names.get(&sid).cloned();
                *sheet = None;
            }
        }
        BoundExpr::Paren(inner) | BoundExpr::SpillRef(inner) => {
            sanitize_rejected_expr(inner, host_sheet, rejected, plan);
        }
        BoundExpr::Unary { expr: inner, .. } => {
            sanitize_rejected_expr(inner, host_sheet, rejected, plan);
        }
        BoundExpr::Binary { left, right, .. } => {
            sanitize_rejected_expr(left, host_sheet, rejected, plan);
            sanitize_rejected_expr(right, host_sheet, rejected, plan);
        }
        BoundExpr::Call { args, .. } => {
            for arg in args {
                sanitize_rejected_expr(arg, host_sheet, rejected, plan);
            }
        }
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Name(_)
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => {}
    }
}

fn formula_is_volatile(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Call { name, args } => {
            reticula_formula::functions::catalog::is_volatile_function(name)
                || args.iter().any(formula_is_volatile)
        }
        BoundExpr::Paren(inner) | BoundExpr::SpillRef(inner) => formula_is_volatile(inner),
        BoundExpr::Unary { expr: inner, .. } => formula_is_volatile(inner),
        BoundExpr::Binary { left, right, .. } => {
            formula_is_volatile(left) || formula_is_volatile(right)
        }
        _ => false,
    }
}

fn format_bound_summary(expr: &BoundExpr) -> String {
    match expr {
        BoundExpr::Call { name, .. } => format!("={name}(...)"),
        BoundExpr::Binary { .. } => "=expression".to_owned(),
        BoundExpr::Reference(_) => "=ref".to_owned(),
        _ => "=formula".to_owned(),
    }
}

fn format_id_rect(sheet: Option<&Sheet>, rows: &[RowId], cols: &[ColId]) -> String {
    let Some(s) = sheet else {
        return format!("{}x{}", rows.len(), cols.len());
    };
    let mut r_min = u32::MAX;
    let mut r_max = 0_u32;
    let mut c_min = u32::MAX;
    let mut c_max = 0_u32;
    for r in rows {
        if let Some(p) = s.row_axis().id_to_pos(r.0) {
            r_min = r_min.min(p);
            r_max = r_max.max(p);
        }
    }
    for c in cols {
        if let Some(p) = s.col_axis().id_to_pos(c.0) {
            c_min = c_min.min(p);
            c_max = c_max.max(p);
        }
    }
    if r_min <= r_max && c_min <= c_max {
        format!("{}:{}", format_a1(r_min, c_min), format_a1(r_max, c_max))
    } else {
        format!("{}x{}", rows.len(), cols.len())
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn describe_structural_op(
    op: &Op<BoundFormula>,
    base: &Workbook<BoundFormula>,
    draft: &Workbook<BoundFormula>,
    main_head: &Workbook<BoundFormula>,
    base_eval: &FormulaEngine,
    main_eval: &FormulaEngine,
    sheet_names: &BTreeMap<SheetId, String>,
) -> (
    SheetId,
    String,
    String,
    BTreeSet<SheetId>,
    BTreeSet<(SheetId, RowId)>,
    BTreeSet<(SheetId, ColId)>,
    Option<String>,
    Vec<CellDiffEntry>,
) {
    let mut created_sheets = BTreeSet::new();
    let mut created_rows = BTreeSet::new();
    let mut created_cols = BTreeSet::new();
    let mut conflict_reason = None;
    let mut conflict_cells = Vec::new();

    match op {
        Op::AddSheet { sheet, name, .. } => {
            created_sheets.insert(*sheet);
            let label = format!("Add worksheet '{name}'");
            (
                *sheet,
                label.clone(),
                label,
                created_sheets,
                created_rows,
                created_cols,
                None,
                conflict_cells,
            )
        }
        Op::DeleteSheet { sheet } => {
            let sname = sheet_names
                .get(sheet)
                .cloned()
                .unwrap_or_else(|| format!("Sheet#{}", sheet.0));
            if let Some(main_s) = main_head.live_sheet(*sheet) {
                for (r, c) in collect_sheet_populated_keys(main_head, main_s) {
                    if logical_cell(main_head, *sheet, r, c) != logical_cell(base, *sheet, r, c) {
                        conflict_reason = Some(format!(
                            "Main branch edited cells on '{sname}' after fork; deleting sheet will discard them"
                        ));
                        break;
                    }
                }
            }
            let label = format!("Delete worksheet '{sname}'");
            (
                *sheet,
                label.clone(),
                label,
                created_sheets,
                created_rows,
                created_cols,
                conflict_reason,
                conflict_cells,
            )
        }
        Op::RenameSheet { sheet, name } => {
            let old = base
                .sheet(*sheet)
                .map(|s| s.name().to_owned())
                .unwrap_or_else(|| format!("Sheet#{}", sheet.0));
            if main_head.live_sheet(*sheet).is_none() {
                conflict_reason = Some("Target worksheet was deleted on main".to_owned());
            }
            let label = format!("Rename worksheet '{old}' -> '{name}'");
            (
                *sheet,
                label.clone(),
                label,
                created_sheets,
                created_rows,
                created_cols,
                conflict_reason,
                conflict_cells,
            )
        }
        Op::MoveSheet { sheet, .. } => {
            let sname = sheet_names
                .get(sheet)
                .cloned()
                .unwrap_or_else(|| format!("Sheet#{}", sheet.0));
            let label = format!("Reorder worksheet '{sname}'");
            (
                *sheet,
                label.clone(),
                label,
                created_sheets,
                created_rows,
                created_cols,
                None,
                conflict_cells,
            )
        }
        Op::InsertRows { sheet, after, ids } => {
            for raw in ids.iter() {
                created_rows.insert((*sheet, RowId(raw)));
            }
            let sname = sheet_names
                .get(sheet)
                .cloned()
                .unwrap_or_else(|| format!("Sheet#{}", sheet.0));
            if main_head.live_sheet(*sheet).is_none() && base.live_sheet(*sheet).is_some() {
                conflict_reason = Some(format!("Worksheet '{sname}' was deleted on main"));
            }
            let pos_hint = draft
                .sheet(*sheet)
                .and_then(|s| s.row_axis().id_to_pos(ids.first_id))
                .map(|p| format!("at row {}", p + 1))
                .or_else(|| after.map(|_| "after anchor row".to_owned()))
                .unwrap_or_else(|| "at top".to_owned());
            let label = format!("Insert {} row(s) on {sname} {pos_hint}", ids.len);
            (
                *sheet,
                label.clone(),
                label,
                created_sheets,
                created_rows,
                created_cols,
                conflict_reason,
                conflict_cells,
            )
        }
        Op::InsertCols { sheet, ids, .. } => {
            for raw in ids.iter() {
                created_cols.insert((*sheet, ColId(raw)));
            }
            let sname = sheet_names
                .get(sheet)
                .cloned()
                .unwrap_or_else(|| format!("Sheet#{}", sheet.0));
            if main_head.live_sheet(*sheet).is_none() && base.live_sheet(*sheet).is_some() {
                conflict_reason = Some(format!("Worksheet '{sname}' was deleted on main"));
            }
            let label = format!("Insert {} column(s) on {sname}", ids.len);
            (
                *sheet,
                label.clone(),
                label,
                created_sheets,
                created_rows,
                created_cols,
                conflict_reason,
                conflict_cells,
            )
        }
        Op::DeleteRows { sheet, ids } => {
            let sname = sheet_names
                .get(sheet)
                .cloned()
                .unwrap_or_else(|| format!("Sheet#{}", sheet.0));
            let del_rows: BTreeSet<RowId> = ids.iter().flat_map(|r| r.iter().map(RowId)).collect();
            if let Some(main_s) = main_head.live_sheet(*sheet) {
                for (r, c) in collect_sheet_populated_keys(main_head, main_s) {
                    if del_rows.contains(&r)
                        && logical_cell(main_head, *sheet, r, c) != logical_cell(base, *sheet, r, c)
                        && let (Some(rp), Some(cp)) = (
                            main_s.row_axis().id_to_pos(r.0),
                            main_s.col_axis().id_to_pos(c.0),
                        )
                    {
                        conflict_reason = Some(
                            "Main branch modified cells on rows deleted by the AI draft".to_owned(),
                        );
                        let (bv, bf) = format_display_and_formula(base_eval, *sheet, r, c);
                        let (mv, mf) = format_display_and_formula(main_eval, *sheet, r, c);
                        conflict_cells.push(CellDiffEntry {
                            cell_key: format_cell_key(*sheet, r, c),
                            sheet_name: sname.clone(),
                            address: format_a1(rp, cp),
                            row_pos: rp,
                            col_pos: cp,
                            base_value: bv,
                            base_formula: bf,
                            draft_value: "<deleted row>".to_owned(),
                            draft_formula: None,
                            main_value: mv,
                            main_formula: mf,
                            conflict: true,
                            default_resolution: ConflictResolution::KeepMain,
                        });
                    }
                }
            }
            let label = format!("Delete {} row(s) on {sname}", del_rows.len());
            (
                *sheet,
                label.clone(),
                label,
                created_sheets,
                created_rows,
                created_cols,
                conflict_reason,
                conflict_cells,
            )
        }
        Op::DeleteCols { sheet, ids } => {
            let sname = sheet_names
                .get(sheet)
                .cloned()
                .unwrap_or_else(|| format!("Sheet#{}", sheet.0));
            let del_cols: BTreeSet<ColId> = ids.iter().flat_map(|c| c.iter().map(ColId)).collect();
            if let Some(main_s) = main_head.live_sheet(*sheet) {
                for (r, c) in collect_sheet_populated_keys(main_head, main_s) {
                    if del_cols.contains(&c)
                        && logical_cell(main_head, *sheet, r, c) != logical_cell(base, *sheet, r, c)
                    {
                        conflict_reason = Some(
                            "Main branch modified cells on columns deleted by the AI draft"
                                .to_owned(),
                        );
                        break;
                    }
                }
            }
            let label = format!("Delete {} column(s) on {sname}", del_cols.len());
            (
                *sheet,
                label.clone(),
                label,
                created_sheets,
                created_rows,
                created_cols,
                conflict_reason,
                conflict_cells,
            )
        }
        Op::MoveRows { sheet, ids, .. } => {
            let sname = sheet_names
                .get(sheet)
                .cloned()
                .unwrap_or_else(|| format!("Sheet#{}", sheet.0));
            let count: u32 = ids.iter().map(|r| r.len).sum();
            let label = format!("Move {count} row(s) on {sname}");
            (
                *sheet,
                label.clone(),
                label,
                created_sheets,
                created_rows,
                created_cols,
                None,
                conflict_cells,
            )
        }
        Op::MoveCols { sheet, ids, .. } => {
            let sname = sheet_names
                .get(sheet)
                .cloned()
                .unwrap_or_else(|| format!("Sheet#{}", sheet.0));
            let count: u32 = ids.iter().map(|c| c.len).sum();
            let label = format!("Move {count} column(s) on {sname}");
            (
                *sheet,
                label.clone(),
                label,
                created_sheets,
                created_rows,
                created_cols,
                None,
                conflict_cells,
            )
        }
        _ => (
            SheetId(0),
            "Structural update".to_owned(),
            "Structural update".to_owned(),
            created_sheets,
            created_rows,
            created_cols,
            None,
            conflict_cells,
        ),
    }
}

/// Clusters sorted cell changes on a single sheet into connected rectangular/adjacent groups.
fn cluster_adjacent_cells(items: Vec<ExistingCellChange>) -> Vec<Vec<ExistingCellChange>> {
    let mut clusters: Vec<Vec<ExistingCellChange>> = Vec::new();
    let mut bounds: Vec<(u32, u32, u32, u32)> = Vec::new(); // (min_r, max_r, min_c, max_c)

    for item in items {
        let (r, c) = (item.0, item.1);
        let mut target = None;
        for (idx, &(r0, r1, c0, c1)) in bounds.iter().enumerate() {
            let near_r = r >= r0.saturating_sub(1) && r <= r1.saturating_add(1);
            let near_c = c >= c0.saturating_sub(1) && c <= c1.saturating_add(1);
            if near_r && near_c {
                target = Some(idx);
                break;
            }
        }
        if let Some(idx) = target {
            let b = &mut bounds[idx];
            b.0 = b.0.min(r);
            b.1 = b.1.max(r);
            b.2 = b.2.min(c);
            b.3 = b.3.max(c);
            clusters[idx].push(item);
        } else {
            bounds.push((r, r, c, c));
            clusters.push(vec![item]);
        }
    }
    clusters
}

#[cfg(test)]
mod tests {
    use reticula_core::id::{ColId, IdAllocator, RowId, SheetId, pack_replica_id};
    use reticula_core::ops::{CellEdit, CellInput, FormatPatch, FormulaMode, Op};
    use reticula_core::value::{CalcError, Value};
    use reticula_formula::binder::BoundFormula;
    use reticula_formula::engine::FormulaEngine;
    use reticula_formula::sort::{SortKey, plan_sort};
    use reticula_wire::convert_v3::ops_to_wire_txn_v3;
    use reticula_wire::v3::WireCommittedTxnV3;

    use super::*;

    fn commit_txn(seq: u64, rep: u32, ops: &[Op<BoundFormula>]) -> WireCommittedTxnV3 {
        WireCommittedTxnV3 {
            seq,
            replica: rep,
            client_seq: seq,
            user_id: [1; 16],
            timestamp_ms: seq * 1000,
            txn: ops_to_wire_txn_v3(format!("step {seq}"), ops).unwrap(),
        }
    }

    #[test]
    fn excludes_volatile_drift_and_detects_three_way_conflicts_without_losing_main_edits() {
        let mut base_eng = FormulaEngine::new();
        let s0 = SheetId(0);
        base_eng
            .edit_cell_raw(s0, RowId(0), ColId(0), "=RAND()")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(1), ColId(0), "10")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(2), ColId(0), "20")
            .unwrap();
        base_eng.recalc_all();
        let base = base_eng.workbook().clone();

        // Main concurrently edits A2 -> 99 and A4 -> 500 (untouched by draft).
        let mut main_eng = FormulaEngine::from_workbook_recalc_all(base.clone());
        main_eng.set_eval_env(46000.0, 999);
        main_eng
            .edit_cell_raw(s0, RowId(1), ColId(0), "99")
            .unwrap();
        main_eng
            .edit_cell_raw(s0, RowId(3), ColId(0), "500")
            .unwrap();
        main_eng.recalc_all();
        let main_head = main_eng.workbook().clone();

        // Draft edits A2 -> 42 (conflict!) and A3 -> 30 (clean), with a different RAND() seed.
        let mut draft_eng = FormulaEngine::from_workbook_recalc_all(base.clone());
        draft_eng.set_eval_env(46000.0, 777);
        draft_eng
            .edit_cell_raw(s0, RowId(1), ColId(0), "42")
            .unwrap();
        draft_eng
            .edit_cell_raw(s0, RowId(2), ColId(0), "30")
            .unwrap();
        draft_eng.recalc_all();
        let draft = draft_eng.workbook().clone();

        let plan = compute_branch_diff(&base, &draft, &main_head, 0, 2, &[]);
        // Volatile A1 (=RAND()) must NOT appear in diff hunks!
        assert!(
            plan.diff
                .hunks
                .iter()
                .all(|h| h.cells.iter().all(|c| c.address != "A1"))
        );
        assert_eq!(plan.diff.summary.conflict_cells, 1);
        assert_eq!(plan.diff.hunks.len(), 2);

        let conflict_hunk = plan.diff.hunks.iter().find(|h| h.has_conflict).unwrap();
        let clean_hunk = plan.diff.hunks.iter().find(|h| !h.has_conflict).unwrap();
        assert_eq!(conflict_hunk.cells[0].address, "A2");
        assert_eq!(clean_hunk.cells[0].address, "A3");

        // KeepMain on conflict A2 + accept clean A3; verify main's A2=99 and A4=500 are preserved!
        let mut choices = BTreeMap::new();
        choices.insert(
            conflict_hunk.cells[0].cell_key.clone(),
            ConflictResolution::KeepMain,
        );
        let sel = MergeSelection {
            accepted_hunks: vec![conflict_hunk.id.clone(), clean_hunk.id.clone()],
            conflict_choices: choices,
            allow_ref_errors: false,
            expected_main_seq: Some("2".to_owned()),
        };
        let merge_rep = pack_replica_id(0, 9).unwrap();
        let batches = plan_merge_ops(&plan, &sel, &main_head, 0, merge_rep, 1).unwrap();
        let mut merged = FormulaEngine::from_workbook_recalc_all(main_head);
        for batch in batches {
            for op in batch {
                merged.apply_op(op).unwrap();
            }
        }
        merged.recalc_all();
        let ms = merged.workbook().sheet(s0).unwrap();
        assert_eq!(ms.value_by_id(RowId(1), ColId(0)), Value::Number(99.0));
        assert_eq!(ms.value_by_id(RowId(2), ColId(0)), Value::Number(30.0));
        assert_eq!(ms.value_by_id(RowId(3), ColId(0)), Value::Number(500.0));
    }

    #[test]
    fn reference_closure_blocks_dangling_row_ref_and_sanitizes_when_allowed() {
        let mut base_eng = FormulaEngine::new();
        let s0 = SheetId(0);
        base_eng
            .edit_cell_raw(s0, RowId(0), ColId(0), "10")
            .unwrap();
        base_eng.recalc_all();
        let base = base_eng.workbook().clone();
        let main_head = base.clone();

        let draft_rep = pack_replica_id(0, 3).unwrap();
        let mut alloc = IdAllocator::new(0, draft_rep, 1).unwrap();
        let new_row_run = alloc.alloc_run(1).unwrap();
        let new_row = RowId(new_row_run.first_id);

        let mut draft_eng = FormulaEngine::from_workbook_recalc_all(base.clone());
        let insert_op = Op::InsertRows {
            sheet: s0,
            after: Some(RowId(0)),
            ids: new_row_run,
        };
        draft_eng.apply_op(insert_op.clone()).unwrap();
        // Put 25 on the inserted row (now row index 1 -> A2).
        draft_eng
            .edit_cell_raw(s0, new_row, ColId(0), "25")
            .unwrap();
        // Put a formula on existing row 0 col 1 (B1) referencing the newly inserted cell A2!
        let f_bound =
            BoundFormula::parse_and_bind("=A2*2", FormulaMode::Dynamic, draft_eng.workbook(), s0)
                .unwrap();
        let write_b1 = Op::SetCells {
            sheet: s0,
            edits: vec![CellEdit {
                row: RowId(0),
                col: ColId(1),
                input: CellInput::Formula {
                    formula: f_bound,
                    cached: None,
                },
                format: FormatPatch::Keep,
            }],
        };
        draft_eng.apply_op(write_b1.clone()).unwrap();
        draft_eng.recalc_all();
        let draft = draft_eng.workbook().clone();

        let txns = vec![
            commit_txn(1, draft_rep, &[insert_op]),
            commit_txn(2, draft_rep, &[write_b1]),
        ];
        let plan = compute_branch_diff(&base, &draft, &main_head, 0, 0, &txns);
        let struct_hunk = plan
            .diff
            .hunks
            .iter()
            .find(|h| h.kind == DiffHunkKind::Structural)
            .unwrap();
        let cell_hunk = plan
            .diff
            .hunks
            .iter()
            .find(|h| h.kind == DiffHunkKind::CellBlock)
            .unwrap();

        // Reject structural hunk, accept B1 formula -> MUST produce closure issue and block merge!
        let sel_blocked = MergeSelection {
            accepted_hunks: vec![cell_hunk.id.clone()],
            conflict_choices: BTreeMap::new(),
            allow_ref_errors: false,
            expected_main_seq: None,
        };
        let issues = check_reference_closure(&plan, &sel_blocked);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].target_hunk_id, struct_hunk.id);
        assert_eq!(issues[0].source_cell, "Sheet1!B1");
        assert_eq!(issues[0].formula_text, "=A2*2");
        assert_eq!(issues[0].ref_display, "Sheet1 row 2");
        let merge_rep = pack_replica_id(0, 8).unwrap();
        assert!(matches!(
            plan_merge_ops(&plan, &sel_blocked, &main_head, 0, merge_rep, 1),
            Err(MergePlanError::ClosureBlocked(_))
        ));

        // Allow #REF! -> rewrites dangling ref cleanly to BoundRef::Invalid evaluating to #REF!.
        let sel_allow = MergeSelection {
            accepted_hunks: vec![cell_hunk.id.clone()],
            conflict_choices: BTreeMap::new(),
            allow_ref_errors: true,
            expected_main_seq: None,
        };
        let batches = plan_merge_ops(&plan, &sel_allow, &main_head, 0, merge_rep, 1).unwrap();
        let mut merged_ref = FormulaEngine::from_workbook_recalc_all(main_head.clone());
        for b in batches {
            for op in b {
                merged_ref.apply_op(op).unwrap();
            }
        }
        merged_ref.recalc_all();
        assert_eq!(
            merged_ref
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(1)),
            Value::Error(CalcError::Ref)
        );

        // Accept BOTH structural hunk and B1 -> remaps IDs cleanly to merge_rep and evaluates to 50!
        let sel_both = MergeSelection {
            accepted_hunks: vec![struct_hunk.id.clone(), cell_hunk.id.clone()],
            conflict_choices: BTreeMap::new(),
            allow_ref_errors: false,
            expected_main_seq: None,
        };
        assert!(check_reference_closure(&plan, &sel_both).is_empty());
        let batches_ok = plan_merge_ops(&plan, &sel_both, &main_head, 0, merge_rep, 1).unwrap();
        let mut merged_ok = FormulaEngine::from_workbook_recalc_all(main_head);
        for b in batches_ok {
            for op in b {
                merged_ok.apply_op(op).unwrap();
            }
        }
        merged_ok.recalc_all();
        assert_eq!(
            merged_ok
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(1)),
            Value::Number(50.0)
        );
    }

    #[test]
    fn sort_hunk_folds_reordered_rows_without_duplicate_cell_block_noise() {
        let mut base_eng = FormulaEngine::new();
        let s0 = SheetId(0);
        base_eng
            .edit_cell_raw(s0, RowId(0), ColId(0), "Name")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(0), ColId(1), "Sales")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(1), ColId(0), "North")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(1), ColId(1), "100")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(2), ColId(0), "South")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(2), ColId(1), "300")
            .unwrap();
        base_eng.recalc_all();
        let base = base_eng.workbook().clone();

        let mut draft_eng = FormulaEngine::from_workbook_recalc_all(base.clone());
        let sort_op = plan_sort(
            draft_eng.workbook().sheet(s0).unwrap(),
            0,
            2,
            0,
            1,
            true,
            &[SortKey {
                col_offset: 1,
                ascending: false,
                case_sensitive: false,
            }],
            None,
        )
        .unwrap();
        draft_eng.apply_op(sort_op.clone()).unwrap();
        draft_eng.recalc_all();

        let draft_rep = pack_replica_id(0, 4).unwrap();
        let txns = vec![commit_txn(1, draft_rep, &[sort_op])];
        let plan = compute_branch_diff(&base, draft_eng.workbook(), &base, 0, 0, &txns);
        assert_eq!(plan.diff.summary.sort_hunks, 1);
        assert_eq!(plan.diff.summary.cell_block_hunks, 0);
        assert_eq!(plan.diff.hunks.len(), 1);
        assert_eq!(plan.diff.hunks[0].kind, DiffHunkKind::Sort);
    }

    #[test]
    fn post_sort_cell_edit_has_no_false_conflict_on_unmodified_main_and_detects_real_conflict() {
        let mut base_eng = FormulaEngine::new();
        let s0 = SheetId(0);
        base_eng
            .edit_cell_raw(s0, RowId(0), ColId(0), "Name")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(0), ColId(1), "Sales")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(1), ColId(0), "North")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(1), ColId(1), "100")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(2), ColId(0), "South")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(2), ColId(1), "300")
            .unwrap();
        base_eng.recalc_all();
        let base = base_eng.workbook().clone();

        // Draft sorts descending (Row 1 becomes South/300, Row 2 becomes North/100)
        // and then edits Row 1's sales (B2) from 300 -> 100 (coincidentally North's old value!).
        let mut draft_eng = FormulaEngine::from_workbook_recalc_all(base.clone());
        let sort_op = plan_sort(
            draft_eng.workbook().sheet(s0).unwrap(),
            0,
            2,
            0,
            1,
            true,
            &[SortKey {
                col_offset: 1,
                ascending: false,
                case_sensitive: false,
            }],
            None,
        )
        .unwrap();
        draft_eng.apply_op(sort_op.clone()).unwrap();
        let edit_op = Op::SetCells {
            sheet: s0,
            edits: vec![CellEdit {
                row: RowId(1),
                col: ColId(1),
                input: CellInput::Scalar(ScalarValue::Number(100.0)),
                format: FormatPatch::Keep,
            }],
        };
        draft_eng.apply_op(edit_op.clone()).unwrap();
        draft_eng.recalc_all();

        let draft_rep = pack_replica_id(0, 4).unwrap();
        let txns = vec![
            commit_txn(1, draft_rep, &[sort_op]),
            commit_txn(2, draft_rep, &[edit_op]),
        ];

        // 1. Against unmodified main: must NOT report a false conflict AND must NOT drop the B2->100 edit!
        let plan_clean = compute_branch_diff(&base, draft_eng.workbook(), &base, 0, 0, &txns);
        assert_eq!(plan_clean.diff.summary.conflict_cells, 0);
        assert_eq!(plan_clean.diff.summary.sort_hunks, 1);
        assert_eq!(plan_clean.diff.summary.cell_block_hunks, 1);
        let cell_hunk = plan_clean
            .diff
            .hunks
            .iter()
            .find(|h| h.kind == DiffHunkKind::CellBlock)
            .unwrap();
        assert_eq!(cell_hunk.cells[0].address, "B2");
        assert_eq!(cell_hunk.cells[0].base_value, "300");
        assert_eq!(cell_hunk.cells[0].main_value, "300");
        assert_eq!(cell_hunk.cells[0].draft_value, "100");

        // 2. Against main that concurrently edited South's row (Row 2: 300 -> 320): MUST report conflict on South!
        let mut main_eng = FormulaEngine::from_workbook_recalc_all(base.clone());
        main_eng
            .edit_cell_raw(s0, RowId(2), ColId(1), "320")
            .unwrap();
        main_eng.recalc_all();
        let plan_conflict = compute_branch_diff(
            &base,
            draft_eng.workbook(),
            main_eng.workbook(),
            0,
            1,
            &txns,
        );
        assert_eq!(plan_conflict.diff.summary.conflict_cells, 1);
        let ch = plan_conflict
            .diff
            .hunks
            .iter()
            .find(|h| h.kind == DiffHunkKind::CellBlock && h.has_conflict)
            .unwrap();
        assert_eq!(ch.cells[0].base_value, "300");
        assert_eq!(ch.cells[0].main_value, "320");
        assert_eq!(ch.cells[0].draft_value, "100");
    }

    #[test]
    fn concurrent_main_row_deletion_flags_conflict_on_cell_edit_and_sort() {
        let mut base_eng = FormulaEngine::new();
        let s0 = SheetId(0);
        base_eng
            .edit_cell_raw(s0, RowId(0), ColId(0), "Header")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(1), ColId(0), "20")
            .unwrap();
        base_eng
            .edit_cell_raw(s0, RowId(2), ColId(0), "10")
            .unwrap();
        base_eng.recalc_all();
        let base = base_eng.workbook().clone();

        // Main deletes Row 1.
        let mut main_eng = FormulaEngine::from_workbook_recalc_all(base.clone());
        main_eng
            .apply_op(Op::DeleteRows {
                sheet: s0,
                ids: vec![IdRun::new(1, 1).unwrap()],
            })
            .unwrap();

        // Draft edits Row 1 -> 99.
        let mut draft_eng = FormulaEngine::from_workbook_recalc_all(base.clone());
        draft_eng
            .edit_cell_raw(s0, RowId(1), ColId(0), "99")
            .unwrap();
        let plan = compute_branch_diff(&base, draft_eng.workbook(), main_eng.workbook(), 0, 1, &[]);
        assert_eq!(plan.diff.summary.conflict_cells, 1);
        let conflict_hunk = plan.diff.hunks.iter().find(|h| h.has_conflict).unwrap();
        assert!(!conflict_hunk.default_accepted);
        assert_eq!(conflict_hunk.cells[0].main_value, "<deleted on main>");

        // Forcing TakeDraft onto a deleted row fails cleanly instead of writing to a tombstone.
        let mut choices = BTreeMap::new();
        choices.insert(
            conflict_hunk.cells[0].cell_key.clone(),
            ConflictResolution::TakeDraft,
        );
        let sel = MergeSelection {
            accepted_hunks: vec![conflict_hunk.id.clone()],
            conflict_choices: choices,
            allow_ref_errors: false,
            expected_main_seq: Some("1".to_owned()),
        };
        let merge_rep = pack_replica_id(0, 7).unwrap();
        assert!(matches!(
            plan_merge_ops(&plan, &sel, main_eng.workbook(), 0, merge_rep, 1),
            Err(MergePlanError::RemapFailed(_))
        ));

        // Sort touching Row 1 also flags SortConflict and defaults unchecked.
        let mut sort_draft = FormulaEngine::from_workbook_recalc_all(base.clone());
        let sort_op = plan_sort(
            sort_draft.workbook().sheet(s0).unwrap(),
            0,
            2,
            0,
            0,
            true,
            &[SortKey {
                col_offset: 0,
                ascending: true,
                case_sensitive: false,
            }],
            None,
        )
        .unwrap();
        sort_draft.apply_op(sort_op.clone()).unwrap();
        let draft_rep = pack_replica_id(0, 5).unwrap();
        let sort_plan = compute_branch_diff(
            &base,
            sort_draft.workbook(),
            main_eng.workbook(),
            0,
            1,
            &[commit_txn(1, draft_rep, &[sort_op])],
        );
        let sort_hunk = sort_plan
            .diff
            .hunks
            .iter()
            .find(|h| h.kind == DiffHunkKind::Sort)
            .unwrap();
        assert!(sort_hunk.has_conflict);
        assert!(!sort_hunk.default_accepted);
    }

    #[test]
    fn structural_to_structural_closure_blocks_insert_on_rejected_new_sheet() {
        let base_eng = FormulaEngine::new();
        let base = base_eng.workbook().clone();
        let draft_rep = pack_replica_id(0, 3).unwrap();
        let new_sid = reticula_core::id::pack_sheet_id(0, draft_rep, 1).unwrap();
        let mut alloc = IdAllocator::new(0, draft_rep, 1).unwrap();
        let new_row = alloc.alloc_run(1).unwrap();

        let add_op = Op::AddSheet {
            sheet: new_sid,
            name: "Summary".to_owned(),
            after: Some(SheetId(0)),
        };
        let ins_op = Op::InsertRows {
            sheet: new_sid,
            after: Some(RowId(0)),
            ids: new_row,
        };
        let mut draft_eng = FormulaEngine::from_workbook_recalc_all(base.clone());
        draft_eng.apply_op(add_op.clone()).unwrap();
        draft_eng.apply_op(ins_op.clone()).unwrap();

        let txns = vec![
            commit_txn(1, draft_rep, &[add_op]),
            commit_txn(2, draft_rep, &[ins_op]),
        ];
        let plan = compute_branch_diff(&base, draft_eng.workbook(), &base, 0, 0, &txns);
        let add_hunk = &plan.diff.hunks[0];
        let ins_hunk = &plan.diff.hunks[1];

        let sel = MergeSelection {
            accepted_hunks: vec![ins_hunk.id.clone()],
            conflict_choices: BTreeMap::new(),
            allow_ref_errors: false,
            expected_main_seq: None,
        };
        let issues = check_reference_closure(&plan, &sel);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].target_hunk_id, add_hunk.id);
    }
}
