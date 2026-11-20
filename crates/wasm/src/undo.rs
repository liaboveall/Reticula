//! Forward-compensating `UndoStack` (`14-m1b.md` 5.1 & `04-engine.md`).
//!
//! Every user transaction produces:
//! - `undo_ops`: forward [`Op`]s that compensate the transaction's effect
//!   (`InsertRows` -> `DeleteRows`, `DeleteRows` -> `UndeleteRows`,
//!   `MoveRows` -> `MoveRows` back to original predecessor + `SetCells`
//!   restoring E25-touched formulas, `FillFormula` -> `ClearRange` + prior
//!   non-empty `SetCells`, etc.);
//! - `redo_ops`: forward [`Op`]s that re-apply the transaction (`InsertRows`
//!   becomes `UndeleteRows`, `AddSheet` becomes `UndeleteSheet`, while
//!   `FillFormula` stays a constant-size `FillFormula` template).
//!
//! Because both undo and redo execute as normal forward [`Op`] transactions,
//! they append directly to OPFS `wal.log` without special-case inverse records.

use std::collections::{BTreeMap, VecDeque};

use reticula_core::axis::AxisRun;
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::ops::{
    CellEdit, CellInput, CellSnapshot, ConditionalCellEdit, FormatBackup, FormatPatch,
    FormatTarget, FormulaPayload, InverseOp, MAX_EDITS_PER_OP, Op, ScalarValue, Workbook,
    WorkbookCellKey,
};
use reticula_core::sheet::Sheet;
use reticula_core::style::{BorderWeight, ColorPatch, HorizontalAlign, StylePatch};
use reticula_formula::binder::BoundFormula;

/// Maximum undo history depth (`14-m1b.md` 5.1).
pub const MAX_UNDO_ENTRIES: usize = 100;

/// Maximum aggregate memory budget of `UndoStack` (`64 MiB`, `14-m1b.md` 5.1).
pub const MAX_UNDO_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// A recorded user transaction with precomputed forward undo and redo
/// operation lists.
#[derive(Clone, Debug)]
pub struct UndoEntry {
    /// Human-readable action description (`"Edit V1"`, `"Fill M2:M1000000"`, ...).
    pub label: String,
    /// Forward compensating operations for `Ctrl+Z`.
    pub undo_ops: Vec<Op<BoundFormula>>,
    /// Forward re-execution operations for `Ctrl+Y` / `Ctrl+Shift+Z`.
    pub redo_ops: Vec<Op<BoundFormula>>,
    /// Estimated heap footprint of this entry.
    pub approx_bytes: usize,
    /// Sequence of the latest collaborative action that produced this entry.
    pub collab_seq: Option<u64>,
}

impl UndoEntry {
    fn measure(&mut self) {
        self.approx_bytes = size_of::<Self>()
            .saturating_add(self.label.capacity())
            .saturating_add(estimate_ops_bytes(&self.undo_ops))
            .saturating_add(estimate_ops_bytes(&self.redo_ops));
    }
}

/// Bounded session undo/redo stack (`<= 100` entries and `<= 64 MiB`).
#[derive(Clone, Debug, Default)]
pub struct UndoStack {
    undo: VecDeque<UndoEntry>,
    redo: Vec<UndoEntry>,
    total_bytes: usize,
}

impl UndoStack {
    /// Creates an empty undo/redo stack.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears all undo and redo entries (used when opening/creating a document).
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.total_bytes = 0;
    }

    /// Whether at least one transaction can be undone.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether at least one undone transaction can be redone.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Label of the next transaction that `Undo` will revert.
    #[must_use]
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.back().map(|e| e.label.as_str())
    }

    /// Label of the next transaction that `Redo` will re-apply.
    #[must_use]
    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|e| e.label.as_str())
    }

    /// Total estimated heap bytes currently held across `undo` and `redo`.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Number of entries currently on the undo stack.
    #[must_use]
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// Synthesizes and pushes an [`UndoEntry`] right after `forward_ops` has
    /// been applied to `post_workbook` (producing `inverses`), clearing any
    /// prior redo branch.
    pub fn record_txn(
        &mut self,
        label: String,
        forward_ops: &[Op<BoundFormula>],
        inverses: Vec<InverseOp<BoundFormula>>,
        post_workbook: &Workbook<BoundFormula>,
    ) {
        self.clear_redo();
        let mut undo_ops = Vec::new();
        for (fwd, inv) in forward_ops.iter().zip(inverses).rev() {
            synthesize_compensating_ops(fwd, inv, post_workbook, &mut undo_ops);
        }
        let redo_ops: Vec<Op<BoundFormula>> = forward_ops.iter().map(to_redo_op).collect();
        self.record_entry(label, undo_ops, redo_ops, None);
    }

    pub(crate) fn record_collab_txn(
        &mut self,
        label: String,
        forward: &[Op<BoundFormula>],
        inverses: Vec<InverseOp<BoundFormula>>,
        workbook: &Workbook<BoundFormula>,
        seq: u64,
    ) {
        self.clear_redo();
        if let Some(undo) = collab_compensations(forward, inverses, workbook) {
            self.record_entry(label, undo, Vec::new(), Some(seq));
        } else {
            self.clear();
        }
    }

    fn record_entry(
        &mut self,
        label: String,
        undo_ops: Vec<Op<BoundFormula>>,
        redo_ops: Vec<Op<BoundFormula>>,
        collab_seq: Option<u64>,
    ) {
        self.push_undo_back(UndoEntry {
            label,
            undo_ops,
            redo_ops,
            approx_bytes: 0,
            collab_seq,
        });
    }

    fn trim(&mut self) {
        while self.undo.len() > MAX_UNDO_ENTRIES || self.total_bytes > MAX_UNDO_TOTAL_BYTES {
            if let Some(oldest) = self.undo.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(oldest.approx_bytes);
            } else {
                self.clear_redo();
                break;
            }
        }
    }

    fn clear_redo(&mut self) {
        for dropped in self.redo.drain(..) {
            self.total_bytes = self.total_bytes.saturating_sub(dropped.approx_bytes);
        }
    }

    pub(crate) fn prune_collab(&mut self, from: u64) {
        self.clear_redo();
        self.undo.retain(|entry| {
            let keep = entry.collab_seq.is_none_or(|seq| seq < from);
            if !keep {
                self.total_bytes = self.total_bytes.saturating_sub(entry.approx_bytes);
            }
            keep
        });
    }

    /// Pops the most recent undo entry for execution; call [`Self::push_redo`]
    /// once the compensating transaction succeeds.
    pub fn pop_undo(&mut self) -> Option<UndoEntry> {
        let entry = self.undo.pop_back()?;
        self.total_bytes = self.total_bytes.saturating_sub(entry.approx_bytes);
        Some(entry)
    }

    /// Moves an executed undo entry onto the redo stack.
    pub fn push_redo(&mut self, mut entry: UndoEntry) {
        entry.measure();
        if entry.approx_bytes > MAX_UNDO_TOTAL_BYTES {
            self.clear();
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(entry.approx_bytes);
        self.redo.push(entry);
        self.trim();
    }

    /// Pops the most recent redo entry for re-execution; call
    /// [`Self::push_undo_back`] once the redo transaction succeeds.
    pub fn pop_redo(&mut self) -> Option<UndoEntry> {
        let entry = self.redo.pop()?;
        self.total_bytes = self.total_bytes.saturating_sub(entry.approx_bytes);
        Some(entry)
    }

    /// Returns an executed redo entry back onto the undo stack.
    pub fn push_undo_back(&mut self, mut entry: UndoEntry) {
        entry.measure();
        if entry.approx_bytes > MAX_UNDO_TOTAL_BYTES {
            self.clear();
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(entry.approx_bytes);
        self.undo.push_back(entry);
        self.trim();
    }
}

/// Compensates only logical cells that were changed. A shadow rewind supplies
/// the expected value immediately after each operation in a compound edit.
pub(crate) fn collab_compensations(
    forward: &[Op<BoundFormula>],
    inverses: Vec<InverseOp<BoundFormula>>,
    workbook: &Workbook<BoundFormula>,
) -> Option<Vec<Op<BoundFormula>>> {
    let mut out = Vec::new();
    let mut shadow = (inverses.len() > 1).then(|| workbook.clone());
    let mut remaining = MAX_UNDO_TOTAL_BYTES;
    for (op, inv) in forward.iter().zip(inverses).rev() {
        let post = shadow.as_ref().unwrap_or(workbook);
        let begin = out.len();
        let rewind = shadow.as_ref().map(|_| inv.clone());
        match inv {
            InverseOp::RestoreCells { sheet, cells } => {
                emit_conditional_restores(post, sheet, cells, remaining, &mut out)?;
            }
            InverseOp::RestoreFilledRange {
                sheet,
                rows,
                cols,
                prior_non_empty,
            } => {
                let row_count = rows.iter().map(|run| run.len as usize).sum::<usize>();
                let col_count = cols.iter().map(|run| run.len as usize).sum::<usize>();
                if row_count
                    .saturating_mul(col_count)
                    .saturating_mul(size_of::<ConditionalCellEdit<BoundFormula>>())
                    > remaining
                {
                    return None;
                }
                let mut prior: BTreeMap<_, _> = prior_non_empty
                    .into_iter()
                    .map(|cell| ((cell.row, cell.col), cell))
                    .collect();
                let cells = rows
                    .into_iter()
                    .flat_map(IdRun::iter)
                    .flat_map(|row| {
                        cols.iter()
                            .copied()
                            .flat_map(IdRun::iter)
                            .map(move |col| (RowId(row), ColId(col)))
                    })
                    .map(|(row, col)| {
                        prior.remove(&(row, col)).unwrap_or(CellSnapshot {
                            row,
                            col,
                            input: CellInput::Empty,
                            explicit_format: None,
                        })
                    });
                emit_conditional_restores(post, sheet, cells, remaining, &mut out)?;
            }
            other => synthesize_compensating_ops(op, other, post, &mut out),
        }
        remaining = remaining.checked_sub(estimate_ops_bytes(&out[begin..]))?;
        if let (Some(shadow), Some(inverse)) = (shadow.as_mut(), rewind) {
            shadow.apply_inverse(inverse).ok()?;
        }
    }
    Some(out)
}

fn emit_conditional_restores(
    post: &Workbook<BoundFormula>,
    sheet: SheetId,
    prior: impl IntoIterator<Item = CellSnapshot<BoundFormula>>,
    mut remaining: usize,
    out: &mut Vec<Op<BoundFormula>>,
) -> Option<()> {
    let mut edits = Vec::new();
    for cell in prior {
        let current = post.cell_snapshot(sheet, cell.row, cell.col)?;
        let expected_input = discard_cache(current.input);
        let new_input = discard_cache(cell.input);
        if expected_input == new_input && current.explicit_format == cell.explicit_format {
            continue;
        }
        remaining = remaining.checked_sub(
            size_of::<ConditionalCellEdit<BoundFormula>>()
                .saturating_add(estimate_cell_input_bytes(&expected_input))
                .saturating_add(estimate_cell_input_bytes(&new_input)),
        )?;
        edits.push(ConditionalCellEdit {
            row: cell.row,
            col: cell.col,
            expected_input,
            expected_format: current.explicit_format,
            new_input,
            new_format: cell
                .explicit_format
                .map_or(FormatPatch::Clear, FormatPatch::Set),
        });
        if edits.len() == MAX_EDITS_PER_OP {
            out.push(Op::SetCellsIf {
                sheet,
                edits: core::mem::take(&mut edits),
            });
        }
    }
    if !edits.is_empty() {
        out.push(Op::SetCellsIf { sheet, edits });
    }
    Some(())
}

fn discard_cache(input: CellInput<BoundFormula>) -> CellInput<BoundFormula> {
    if let CellInput::Formula { formula, .. } = input {
        CellInput::Formula {
            formula,
            cached: None,
        }
    } else {
        input
    }
}

fn to_redo_op(op: &Op<BoundFormula>) -> Op<BoundFormula> {
    match op {
        Op::InsertRows { sheet, ids, .. } => Op::UndeleteRows {
            sheet: *sheet,
            ids: vec![*ids],
        },
        Op::InsertCols { sheet, ids, .. } => Op::UndeleteCols {
            sheet: *sheet,
            ids: vec![*ids],
        },
        Op::AddSheet { sheet, .. } => Op::UndeleteSheet { sheet: *sheet },
        other => other.clone(),
    }
}

fn synthesize_compensating_ops(
    forward_op: &Op<BoundFormula>,
    inverse: InverseOp<BoundFormula>,
    post_wb: &Workbook<BoundFormula>,
    out: &mut Vec<Op<BoundFormula>>,
) {
    match inverse {
        InverseOp::RestoreCells { sheet, cells } => {
            emit_snapshot_restore_chunks(sheet, cells, out);
        }
        InverseOp::RestoreFilledRange {
            sheet,
            rows,
            cols,
            prior_non_empty,
        } => {
            out.push(Op::ClearRange { sheet, rows, cols });
            emit_snapshot_restore_chunks(sheet, prior_non_empty, out);
        }
        InverseOp::RemoveInsertedRows { sheet, ids } => {
            out.push(Op::DeleteRows {
                sheet,
                ids: vec![ids],
            });
        }
        InverseOp::RemoveInsertedCols { sheet, ids } => {
            out.push(Op::DeleteCols {
                sheet,
                ids: vec![ids],
            });
        }
        InverseOp::UndeleteRows { sheet, ids } => {
            out.push(Op::UndeleteRows { sheet, ids });
        }
        InverseOp::UndeleteCols { sheet, ids } => {
            out.push(Op::UndeleteCols { sheet, ids });
        }
        InverseOp::RedeleteRows { sheet, ids } => {
            out.push(Op::DeleteRows { sheet, ids });
        }
        InverseOp::RedeleteCols { sheet, ids } => {
            out.push(Op::DeleteCols { sheet, ids });
        }
        InverseOp::RestoreMovedRows {
            sheet,
            prior_runs,
            rewritten,
        } => {
            let moved_ids = match forward_op {
                Op::MoveRows { ids, .. } => ids.as_slice(),
                _ => &[],
            };
            for (group_runs, after_raw) in moved_blocks_with_predecessors(&prior_runs, moved_ids) {
                out.push(Op::MoveRows {
                    sheet,
                    ids: group_runs,
                    after: after_raw.map(RowId),
                });
            }
            let mut formula_map: BTreeMap<WorkbookCellKey, BoundFormula> = BTreeMap::new();
            for (k, f) in post_wb.formulas_touching_row_endpoints(sheet, moved_ids) {
                formula_map.insert(k, f);
            }
            for (k, prior_f) in rewritten {
                formula_map.insert(k, prior_f);
            }
            emit_formula_restores(post_wb, formula_map, out);
        }
        InverseOp::RestoreMovedCols {
            sheet,
            prior_runs,
            rewritten,
        } => {
            let moved_ids = match forward_op {
                Op::MoveCols { ids, .. } => ids.as_slice(),
                _ => &[],
            };
            for (group_runs, after_raw) in moved_blocks_with_predecessors(&prior_runs, moved_ids) {
                out.push(Op::MoveCols {
                    sheet,
                    ids: group_runs,
                    after: after_raw.map(ColId),
                });
            }
            let mut formula_map: BTreeMap<WorkbookCellKey, BoundFormula> = BTreeMap::new();
            for (k, f) in post_wb.formulas_touching_col_endpoints(sheet, moved_ids) {
                formula_map.insert(k, f);
            }
            for (k, prior_f) in rewritten {
                formula_map.insert(k, prior_f);
            }
            emit_formula_restores(post_wb, formula_map, out);
        }
        InverseOp::RestoreRowHeights { sheet, prior } => {
            let sizes = coalesce_row_sizes(&prior);
            if !sizes.is_empty() {
                out.push(Op::ResizeRows { sheet, sizes });
            }
        }
        InverseOp::RestoreColWidths { sheet, prior } => {
            let sizes = coalesce_col_sizes(&prior);
            if !sizes.is_empty() {
                out.push(Op::ResizeCols { sheet, sizes });
            }
        }
        InverseOp::RestoreFreeze {
            sheet,
            frozen_rows,
            frozen_cols,
        } => {
            out.push(Op::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            });
        }
        InverseOp::RestoreFormat { sheet, backup } => {
            let (fwd_nf, fwd_style) = match forward_op {
                Op::SetFormat {
                    number_format,
                    style,
                    ..
                } => (*number_format, *style),
                _ => (FormatPatch::Keep, StylePatch::default()),
            };
            synthesize_format_restore(sheet, backup, fwd_nf, fwd_style, post_wb.sheet(sheet), out);
        }
        InverseOp::RemoveAddedSheet { sheet, .. } => {
            out.push(Op::DeleteSheet { sheet });
        }
        InverseOp::RestoreSheetName { sheet, prior_name } => {
            out.push(Op::RenameSheet {
                sheet,
                name: prior_name,
            });
        }
        InverseOp::UndeleteSheet { sheet } => {
            out.push(Op::UndeleteSheet { sheet });
        }
        InverseOp::RedeleteSheet { sheet, prior_name } => {
            let cur_name = post_wb.sheet(sheet).map(Sheet::name).unwrap_or("");
            if cur_name != prior_name {
                out.push(Op::RenameSheet {
                    sheet,
                    name: prior_name,
                });
            }
            out.push(Op::DeleteSheet { sheet });
        }
        InverseOp::RestoreSheetOrder { order } => {
            if let Op::MoveSheet { sheet, .. } = forward_op
                && let Some(orig_idx) = order.iter().position(|id| id == sheet)
            {
                let after = orig_idx.checked_sub(1).and_then(|i| order.get(i).copied());
                out.push(Op::MoveSheet {
                    sheet: *sheet,
                    after,
                });
            }
        }
    }
}

fn emit_snapshot_restore_chunks(
    sheet: SheetId,
    snapshots: Vec<CellSnapshot<BoundFormula>>,
    out: &mut Vec<Op<BoundFormula>>,
) {
    if snapshots.is_empty() {
        return;
    }
    let mut chunk = Vec::with_capacity(snapshots.len().min(MAX_EDITS_PER_OP));
    for snap in snapshots {
        let format = match snap.explicit_format {
            Some(fmt) => FormatPatch::Set(fmt),
            None => FormatPatch::Clear,
        };
        chunk.push(CellEdit {
            row: snap.row,
            col: snap.col,
            input: snap.input,
            format,
        });
        if chunk.len() == MAX_EDITS_PER_OP {
            out.push(Op::SetCells {
                sheet,
                edits: core::mem::take(&mut chunk),
            });
        }
    }
    if !chunk.is_empty() {
        out.push(Op::SetCells {
            sheet,
            edits: chunk,
        });
    }
}

fn emit_formula_restores(
    post_wb: &Workbook<BoundFormula>,
    formulas: BTreeMap<WorkbookCellKey, BoundFormula>,
    out: &mut Vec<Op<BoundFormula>>,
) {
    let mut by_sheet: BTreeMap<SheetId, Vec<CellEdit<BoundFormula>>> = BTreeMap::new();
    for ((sheet, row, col), formula) in formulas {
        let cached = post_wb
            .cell_snapshot(sheet, row, col)
            .and_then(|snap| match snap.input {
                CellInput::Formula { cached, .. } => cached,
                CellInput::Scalar(s) => Some(s),
                CellInput::Empty => None,
            });
        by_sheet.entry(sheet).or_default().push(CellEdit {
            row,
            col,
            input: CellInput::Formula { formula, cached },
            format: FormatPatch::Keep,
        });
    }
    for (sheet, edits) in by_sheet {
        for chunk in edits.chunks(MAX_EDITS_PER_OP) {
            out.push(Op::SetCells {
                sheet,
                edits: chunk.to_vec(),
            });
        }
    }
}

/// Splits `prior_runs` at moved-run boundaries in `O(runs + moved.len())` time
/// (never iterating individual IDs of million-row base runs) and groups
/// contiguous moved sub-runs with the stationary ID that immediately preceded
/// them in `prior_runs`.
fn moved_blocks_with_predecessors(
    prior_runs: &[AxisRun],
    moved: &[IdRun],
) -> Vec<(Vec<IdRun>, Option<u64>)> {
    if moved.is_empty() {
        return Vec::new();
    }

    // Sub-runs `(IdRun, is_moved)`.
    let mut pieces: Vec<(IdRun, bool)> = Vec::new();
    for axis_run in prior_runs {
        let run_start = axis_run.ids.first_id;
        let Some(run_end_excl) = run_start.checked_add(u64::from(axis_run.ids.len)) else {
            continue;
        };
        let mut cuts = vec![run_start, run_end_excl];
        for m in moved {
            let ms = m.first_id;
            let me = m.first_id.saturating_add(u64::from(m.len));
            if ms > run_start && ms < run_end_excl {
                cuts.push(ms);
            }
            if me > run_start && me < run_end_excl {
                cuts.push(me);
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for win in cuts.windows(2) {
            let &[a, b] = win else { continue };
            if let Ok(len) = u32::try_from(b.saturating_sub(a))
                && len > 0
            {
                let is_moved = moved.iter().any(|m| m.contains(a));
                pieces.push((IdRun { first_id: a, len }, is_moved));
            }
        }
    }

    let mut groups: Vec<(Vec<IdRun>, Option<u64>)> = Vec::new();
    let mut last_stationary_id: Option<u64> = None;
    let mut active_moved: Vec<IdRun> = Vec::new();
    let mut active_pred: Option<u64> = None;

    for (sub_run, is_moved) in pieces {
        if is_moved {
            if active_moved.is_empty() {
                active_pred = last_stationary_id;
            }
            if let Some(prev) = active_moved.last_mut()
                && prev.first_id.checked_add(u64::from(prev.len)) == Some(sub_run.first_id)
                && let Some(merged_len) = prev.len.checked_add(sub_run.len)
            {
                prev.len = merged_len;
            } else {
                active_moved.push(sub_run);
            }
        } else {
            if !active_moved.is_empty() {
                groups.push((core::mem::take(&mut active_moved), active_pred));
            }
            last_stationary_id = sub_run.id_at(sub_run.len.saturating_sub(1));
        }
    }
    if !active_moved.is_empty() {
        groups.push((active_moved, active_pred));
    }
    groups
}

fn coalesce_row_sizes(prior: &[(RowId, Option<u16>)]) -> Vec<(IdRun, Option<u16>)> {
    let mut out: Vec<(IdRun, Option<u16>)> = Vec::new();
    for &(row, px) in prior {
        if let Some((last_run, last_px)) = out.last_mut()
            && *last_px == px
            && last_run.first_id.checked_add(u64::from(last_run.len)) == Some(row.0)
            && let Some(new_len) = last_run.len.checked_add(1)
        {
            last_run.len = new_len;
            continue;
        }
        out.push((
            IdRun {
                first_id: row.0,
                len: 1,
            },
            px,
        ));
    }
    out
}

fn coalesce_col_sizes(prior: &[(ColId, Option<u16>)]) -> Vec<(IdRun, Option<u16>)> {
    let mut out: Vec<(IdRun, Option<u16>)> = Vec::new();
    for &(col, px) in prior {
        if let Some((last_run, last_px)) = out.last_mut()
            && *last_px == px
            && last_run.first_id.checked_add(u64::from(last_run.len)) == Some(col.0)
            && let Some(new_len) = last_run.len.checked_add(1)
        {
            last_run.len = new_len;
            continue;
        }
        out.push((
            IdRun {
                first_id: col.0,
                len: 1,
            },
            px,
        ));
    }
    out
}

fn synthesize_format_restore(
    sheet_id: SheetId,
    backup: FormatBackup,
    fwd_nf: FormatPatch,
    fwd_style: StylePatch,
    sheet_opt: Option<&Sheet>,
    out: &mut Vec<Op<BoundFormula>>,
) {
    let nf_changed = !matches!(fwd_nf, FormatPatch::Keep);
    match backup {
        FormatBackup::Cols(entries) => {
            let mut cur: Option<(IdRun, FormatPatch, StylePatch)> = None;
            for (col, old_fmt, old_st) in entries {
                let nf = if !nf_changed {
                    FormatPatch::Keep
                } else if old_fmt == NumberFormat::General {
                    FormatPatch::Clear
                } else {
                    FormatPatch::Set(old_fmt)
                };
                let st = restore_style_fields(fwd_style, old_st, StylePatch::default());
                if let Some((ref mut run, cur_nf, cur_st)) = cur
                    && cur_nf == nf
                    && cur_st == st
                    && run.first_id.checked_add(u64::from(run.len)) == Some(col.0)
                    && let Some(nl) = run.len.checked_add(1)
                {
                    run.len = nl;
                    continue;
                }
                if let Some((run, cur_nf, cur_st)) = cur.take() {
                    out.push(Op::SetFormat {
                        sheet: sheet_id,
                        target: FormatTarget::Cols(vec![run]),
                        number_format: cur_nf,
                        style: cur_st,
                    });
                }
                cur = Some((
                    IdRun {
                        first_id: col.0,
                        len: 1,
                    },
                    nf,
                    st,
                ));
            }
            if let Some((run, cur_nf, cur_st)) = cur {
                out.push(Op::SetFormat {
                    sheet: sheet_id,
                    target: FormatTarget::Cols(vec![run]),
                    number_format: cur_nf,
                    style: cur_st,
                });
            }
        }
        FormatBackup::Rows(entries) => {
            let mut cur: Option<(IdRun, FormatPatch, StylePatch)> = None;
            for (row, old_fmt, old_st) in entries {
                let nf = if !nf_changed {
                    FormatPatch::Keep
                } else {
                    match old_fmt {
                        Some(f) => FormatPatch::Set(f),
                        None => FormatPatch::Clear,
                    }
                };
                let st = restore_style_fields(fwd_style, old_st, StylePatch::default());
                if let Some((ref mut run, cur_nf, cur_st)) = cur
                    && cur_nf == nf
                    && cur_st == st
                    && run.first_id.checked_add(u64::from(run.len)) == Some(row.0)
                    && let Some(nl) = run.len.checked_add(1)
                {
                    run.len = nl;
                    continue;
                }
                if let Some((run, cur_nf, cur_st)) = cur.take() {
                    out.push(Op::SetFormat {
                        sheet: sheet_id,
                        target: FormatTarget::Rows(vec![run]),
                        number_format: cur_nf,
                        style: cur_st,
                    });
                }
                cur = Some((
                    IdRun {
                        first_id: row.0,
                        len: 1,
                    },
                    nf,
                    st,
                ));
            }
            if let Some((run, cur_nf, cur_st)) = cur {
                out.push(Op::SetFormat {
                    sheet: sheet_id,
                    target: FormatTarget::Rows(vec![run]),
                    number_format: cur_nf,
                    style: cur_st,
                });
            }
        }
        FormatBackup::Cells(entries) => {
            // Group cells by `(col, nf, st)` into row runs, then coalesce matching
            // row-run vectors across consecutive columns!
            let mut by_col: BTreeMap<u64, Vec<(u64, FormatPatch, StylePatch)>> = BTreeMap::new();
            for (row, col, old_fmt, old_st) in entries {
                let nf = if !nf_changed {
                    FormatPatch::Keep
                } else {
                    match old_fmt {
                        Some(f) => FormatPatch::Set(f),
                        None => FormatPatch::Clear,
                    }
                };
                let inherited = sheet_opt.map_or_else(StylePatch::default, |s| {
                    s.row_style_patch(row).merged_over(s.col_style_patch(col))
                });
                let st = restore_style_fields(fwd_style, old_st, inherited);
                by_col.entry(col.0).or_default().push((row.0, nf, st));
            }

            let mut groups: Vec<(Vec<IdRun>, Vec<IdRun>, FormatPatch, StylePatch)> = Vec::new();
            for (col_raw, mut col_entries) in by_col {
                col_entries.sort_unstable_by_key(|&(r, _, _)| r);
                // Coalesce consecutive rows in this column with identical `(nf, st)`.
                let mut col_runs: Vec<(IdRun, FormatPatch, StylePatch)> = Vec::new();
                for (r_raw, nf, st) in col_entries {
                    if let Some((last_r, last_nf, last_st)) = col_runs.last_mut()
                        && *last_nf == nf
                        && *last_st == st
                        && last_r.first_id.checked_add(u64::from(last_r.len)) == Some(r_raw)
                        && let Some(nl) = last_r.len.checked_add(1)
                    {
                        last_r.len = nl;
                        continue;
                    }
                    col_runs.push((
                        IdRun {
                            first_id: r_raw,
                            len: 1,
                        },
                        nf,
                        st,
                    ));
                }

                // Merge runs of the same `(nf, st)` within this column.
                let mut by_spec: Vec<(FormatPatch, StylePatch, Vec<IdRun>)> = Vec::new();
                for (r_run, nf, st) in col_runs {
                    if let Some(found) = by_spec
                        .iter_mut()
                        .find(|(f_nf, f_st, _)| *f_nf == nf && *f_st == st)
                    {
                        found.2.push(r_run);
                    } else {
                        by_spec.push((nf, st, vec![r_run]));
                    }
                }

                for (nf, st, r_runs) in by_spec {
                    if let Some(existing) =
                        groups.iter_mut().find(|(g_rows, g_cols, g_nf, g_st)| {
                            *g_nf == nf
                                && *g_st == st
                                && *g_rows == r_runs
                                && g_cols.last().is_some_and(|lc| {
                                    lc.first_id.checked_add(u64::from(lc.len)) == Some(col_raw)
                                })
                        })
                    {
                        if let Some(lc) = existing.1.last_mut() {
                            lc.len = lc.len.saturating_add(1);
                        }
                    } else {
                        groups.push((
                            r_runs,
                            vec![IdRun {
                                first_id: col_raw,
                                len: 1,
                            }],
                            nf,
                            st,
                        ));
                    }
                }
            }

            for (rows, cols, number_format, style) in groups {
                out.push(Op::SetFormat {
                    sheet: sheet_id,
                    target: FormatTarget::Cells { rows, cols },
                    number_format,
                    style,
                });
            }
        }
    }
}

fn restore_style_fields(fwd: StylePatch, old: StylePatch, inherited: StylePatch) -> StylePatch {
    StylePatch {
        bold: fwd
            .bold
            .map(|_| old.bold.or(inherited.bold).unwrap_or(false)),
        italic: fwd
            .italic
            .map(|_| old.italic.or(inherited.italic).unwrap_or(false)),
        strikethrough: fwd.strikethrough.map(|_| {
            old.strikethrough
                .or(inherited.strikethrough)
                .unwrap_or(false)
        }),
        align: fwd.align.map(|_| {
            old.align
                .or(inherited.align)
                .unwrap_or(HorizontalAlign::Auto)
        }),
        text_color: if matches!(fwd.text_color, ColorPatch::Keep) {
            ColorPatch::Keep
        } else {
            match old.text_color {
                ColorPatch::Set(c) => ColorPatch::Set(c),
                ColorPatch::Clear => ColorPatch::Clear,
                ColorPatch::Keep => match inherited.text_color {
                    ColorPatch::Set(c) => ColorPatch::Set(c),
                    ColorPatch::Clear | ColorPatch::Keep => ColorPatch::Clear,
                },
            }
        },
        fill_color: if matches!(fwd.fill_color, ColorPatch::Keep) {
            ColorPatch::Keep
        } else {
            match old.fill_color {
                ColorPatch::Set(c) => ColorPatch::Set(c),
                ColorPatch::Clear => ColorPatch::Clear,
                ColorPatch::Keep => match inherited.fill_color {
                    ColorPatch::Set(c) => ColorPatch::Set(c),
                    ColorPatch::Clear | ColorPatch::Keep => ColorPatch::Clear,
                },
            }
        },
        border_top: fwd.border_top.map(|_| {
            old.border_top
                .or(inherited.border_top)
                .unwrap_or(BorderWeight::None)
        }),
        border_right: fwd.border_right.map(|_| {
            old.border_right
                .or(inherited.border_right)
                .unwrap_or(BorderWeight::None)
        }),
        border_bottom: fwd.border_bottom.map(|_| {
            old.border_bottom
                .or(inherited.border_bottom)
                .unwrap_or(BorderWeight::None)
        }),
        border_left: fwd.border_left.map(|_| {
            old.border_left
                .or(inherited.border_left)
                .unwrap_or(BorderWeight::None)
        }),
    }
}

fn estimate_ops_bytes(ops: &[Op<BoundFormula>]) -> usize {
    ops.iter().fold(
        ops.len().saturating_mul(size_of::<Op<BoundFormula>>()),
        |acc, op| acc.saturating_add(estimate_single_op_bytes(op)),
    )
}

fn estimate_single_op_bytes(op: &Op<BoundFormula>) -> usize {
    match op {
        Op::SetCells { edits, .. } => edits.iter().fold(
            edits
                .capacity()
                .saturating_mul(size_of::<CellEdit<BoundFormula>>()),
            |acc, e| acc.saturating_add(estimate_cell_input_bytes(&e.input)),
        ),
        Op::SetCellsIf { edits, .. } => edits.iter().fold(
            edits.capacity().saturating_mul(size_of::<
                reticula_core::ops::ConditionalCellEdit<BoundFormula>,
            >()),
            |acc, e| {
                acc.saturating_add(estimate_cell_input_bytes(&e.expected_input))
                    .saturating_add(estimate_cell_input_bytes(&e.new_input))
            },
        ),
        Op::ClearRange { rows, cols, .. } => rows
            .capacity()
            .saturating_add(cols.capacity())
            .saturating_mul(size_of::<IdRun>()),
        Op::InsertRows { .. }
        | Op::InsertCols { .. }
        | Op::SetFreeze { .. }
        | Op::DeleteSheet { .. }
        | Op::UndeleteSheet { .. }
        | Op::MoveSheet { .. } => 32,
        Op::DeleteRows { ids, .. }
        | Op::DeleteCols { ids, .. }
        | Op::UndeleteRows { ids, .. }
        | Op::UndeleteCols { ids, .. }
        | Op::MoveRows { ids, .. }
        | Op::MoveCols { ids, .. } => ids.capacity().saturating_mul(size_of::<IdRun>()),
        Op::ResizeRows { sizes, .. } | Op::ResizeCols { sizes, .. } => sizes
            .capacity()
            .saturating_mul(size_of::<(IdRun, Option<u16>)>()),
        Op::SetFormat { target, .. } => match target {
            FormatTarget::Cells { rows, cols } => rows
                .capacity()
                .saturating_add(cols.capacity())
                .saturating_mul(size_of::<IdRun>()),
            FormatTarget::Rows(r) | FormatTarget::Cols(r) => {
                r.capacity().saturating_mul(size_of::<IdRun>())
            }
        },
        Op::SetBlock {
            rows,
            cols,
            columns_data,
            ..
        } => {
            let base = rows
                .capacity()
                .saturating_mul(size_of::<IdRun>())
                .saturating_add(cols.capacity().saturating_mul(size_of::<ColId>()));
            columns_data.iter().fold(base, |acc, col| {
                col.iter().fold(
                    acc.saturating_add(
                        col.capacity()
                            .saturating_mul(size_of::<CellInput<BoundFormula>>()),
                    ),
                    |inner, cell| inner.saturating_add(estimate_cell_input_bytes(cell)),
                )
            })
        }
        Op::FillFormula {
            rows,
            cols,
            template,
            ..
        } => rows
            .capacity()
            .saturating_add(cols.capacity())
            .saturating_mul(size_of::<IdRun>())
            .saturating_add(BoundFormula::validate_template(template).unwrap_or(256)),
        Op::AddSheet { name, .. } | Op::RenameSheet { name, .. } => name.capacity(),
    }
}

fn estimate_cell_input_bytes(input: &CellInput<BoundFormula>) -> usize {
    match input {
        CellInput::Empty => 0,
        CellInput::Scalar(s) => match s {
            ScalarValue::Text(t) => t.capacity(),
            ScalarValue::Number(_) | ScalarValue::Bool(_) | ScalarValue::Error(_) => 16,
        },
        CellInput::Formula { formula, cached } => {
            formula
                .estimated_bytes()
                .saturating_add(cached.as_ref().map_or(0, |c| {
                    if let ScalarValue::Text(t) = c {
                        t.capacity()
                    } else {
                        16
                    }
                }))
        }
    }
}

#[cfg(test)]
mod tests {
    use reticula_core::format::NumberFormat;
    use reticula_core::id::{ColId, IdRun, RowId, SheetId};
    use reticula_core::ops::{FormatPatch, FormatTarget, Op};
    use reticula_core::style::{ColorPatch, ColorValue, StylePatch};
    use reticula_core::value::Value;
    use reticula_formula::binder::{FillTargetRect, build_fill_plan};
    use reticula_formula::engine::FormulaEngine;

    use super::{MAX_UNDO_ENTRIES, UndoStack};

    fn exec_txn(
        engine: &mut FormulaEngine,
        stack: &mut UndoStack,
        label: &str,
        ops: Vec<Op<reticula_formula::binder::BoundFormula>>,
    ) {
        let saved = ops.clone();
        let inverses = engine.apply_txn(ops).expect("apply_txn");
        engine.recalc_all();
        stack.record_txn(String::from(label), &saved, inverses, engine.workbook());
    }

    fn undo_once(engine: &mut FormulaEngine, stack: &mut UndoStack) {
        let entry = stack.pop_undo().expect("undo entry");
        let _ = engine.apply_txn(entry.undo_ops.clone()).expect("undo txn");
        engine.recalc_all();
        stack.push_redo(entry);
    }

    fn redo_once(engine: &mut FormulaEngine, stack: &mut UndoStack) {
        let entry = stack.pop_redo().expect("redo entry");
        let _ = engine.apply_txn(entry.redo_ops.clone()).expect("redo txn");
        engine.recalc_all();
        stack.push_undo_back(entry);
    }

    #[test]
    fn undo_redo_preserves_e25_inward_and_outward_row_moves_and_insert_delete() {
        let mut engine = FormulaEngine::new();
        let mut stack = UndoStack::new();
        let s0 = SheetId(0);

        // Populate A2..A5 = 10, 20, 30, 40 and B1 = `=SUM(A2:A5)` (= 100).
        for (r, v) in [(1_u64, 10.0), (2, 20.0), (3, 30.0), (4, 40.0)] {
            let inv = engine
                .edit_cell_raw(s0, RowId(r), ColId(0), &format!("{v}"))
                .unwrap();
            let _ = inv;
        }
        let _ = engine
            .edit_cell_raw(s0, RowId(0), ColId(1), "=SUM(A2:A5)")
            .unwrap();
        engine.recalc_all();
        assert_eq!(
            engine.workbook().sheet(s0).unwrap().value(0, 1),
            Value::Number(100.0)
        );

        // 1. Outward stretch: move row 5 (`RowId(4)`) down after row 8 (`RowId(7)`).
        exec_txn(
            &mut engine,
            &mut stack,
            "Move row outward",
            vec![Op::MoveRows {
                sheet: s0,
                ids: vec![IdRun {
                    first_id: 4,
                    len: 1,
                }],
                after: Some(RowId(7)),
            }],
        );
        assert_eq!(
            engine.inspect_cell(s0, RowId(0), ColId(1)).unwrap().raw,
            "=SUM(A2:A8)"
        );

        // Undo outward move -> must restore `=SUM(A2:A5)` (NOT hand off to A8!).
        undo_once(&mut engine, &mut stack);
        assert_eq!(
            engine.inspect_cell(s0, RowId(0), ColId(1)).unwrap().raw,
            "=SUM(A2:A5)"
        );
        assert_eq!(
            engine.workbook().sheet(s0).unwrap().value(0, 1),
            Value::Number(100.0)
        );

        // Redo outward move -> back to `=SUM(A2:A8)`.
        redo_once(&mut engine, &mut stack);
        assert_eq!(
            engine.inspect_cell(s0, RowId(0), ColId(1)).unwrap().raw,
            "=SUM(A2:A8)"
        );
        undo_once(&mut engine, &mut stack);

        // 2. Inward handoff: move row 5 (`RowId(4)`) up to top (`after: None`).
        exec_txn(
            &mut engine,
            &mut stack,
            "Move row inward",
            vec![Op::MoveRows {
                sheet: s0,
                ids: vec![IdRun {
                    first_id: 4,
                    len: 1,
                }],
                after: None,
            }],
        );
        // `RowId(4)` moved above the range, so bottom handed off to `RowId(3)` (`A3:A5`, sum = 60).
        assert_eq!(
            engine.workbook().sheet(s0).unwrap().value(1, 1),
            Value::Number(60.0)
        );

        // Undo inward move -> restores `=SUM(A2:A5)` = 100!
        undo_once(&mut engine, &mut stack);
        assert_eq!(
            engine.inspect_cell(s0, RowId(0), ColId(1)).unwrap().raw,
            "=SUM(A2:A5)"
        );
        assert_eq!(
            engine.workbook().sheet(s0).unwrap().value(0, 1),
            Value::Number(100.0)
        );

        // 3. InsertRows -> Undo (`DeleteRows`) -> Redo (`UndeleteRows`).
        let ins_ids = engine.allocator_mut().alloc_run(2).unwrap();
        exec_txn(
            &mut engine,
            &mut stack,
            "Insert 2 rows",
            vec![Op::InsertRows {
                sheet: s0,
                after: Some(RowId(1)),
                ids: ins_ids,
            }],
        );
        assert_eq!(
            engine.inspect_cell(s0, RowId(0), ColId(1)).unwrap().raw,
            "=SUM(A2:A7)"
        );
        undo_once(&mut engine, &mut stack);
        assert_eq!(
            engine.inspect_cell(s0, RowId(0), ColId(1)).unwrap().raw,
            "=SUM(A2:A5)"
        );
        redo_once(&mut engine, &mut stack);
        assert_eq!(
            engine.inspect_cell(s0, RowId(0), ColId(1)).unwrap().raw,
            "=SUM(A2:A7)"
        );
    }

    #[test]
    fn fill_formula_1m_rows_has_tiny_undo_and_redo_footprint_and_sheet_ops_round_trip() {
        let mut engine = FormulaEngine::new();
        let mut stack = UndoStack::new();
        let s0 = SheetId(0);

        let _ = engine
            .edit_cell_raw(s0, RowId(0), ColId(0), "=ROW()*2")
            .unwrap();
        engine.recalc_all();
        let src_f = engine
            .workbook()
            .formula(s0, RowId(0), ColId(0))
            .unwrap()
            .clone();
        let plan = build_fill_plan(
            &src_f,
            engine.workbook(),
            s0,
            0,
            0,
            FillTargetRect {
                dst_sheet: s0,
                first_row_pos: 1,
                row_count: 100_000,
                first_col_pos: 0,
                col_count: 1,
            },
        );
        exec_txn(&mut engine, &mut stack, "Fill 100k", plan);
        // Filling 100,000 blank rows keeps undo+redo entry tiny (< 2 KiB)!
        assert!(stack.total_bytes() < 2048, "bytes={}", stack.total_bytes());

        // Format + AddSheet + DeleteSheet undo/redo.
        exec_txn(
            &mut engine,
            &mut stack,
            "Format A1",
            vec![Op::SetFormat {
                sheet: s0,
                target: FormatTarget::Cells {
                    rows: vec![IdRun {
                        first_id: 0,
                        len: 1,
                    }],
                    cols: vec![IdRun {
                        first_id: 0,
                        len: 1,
                    }],
                },
                number_format: FormatPatch::Set(NumberFormat::Fixed2),
                style: StylePatch {
                    bold: Some(true),
                    fill_color: ColorPatch::Set(ColorValue::Palette(2)),
                    ..StylePatch::default()
                },
            }],
        );
        let s1 = engine.alloc_sheet_id();
        exec_txn(
            &mut engine,
            &mut stack,
            "Add Sheet2",
            vec![Op::AddSheet {
                sheet: s1,
                name: String::from("Sheet2"),
                after: Some(s0),
            }],
        );
        assert_eq!(engine.workbook().live_sheets().count(), 2);

        // New edit after undo clears redo branch; cap at MAX_UNDO_ENTRIES (100).
        undo_once(&mut engine, &mut stack); // undoes AddSheet -> DeleteSheet(s1)
        assert_eq!(engine.workbook().live_sheets().count(), 1);
        assert!(stack.can_redo());
        redo_once(&mut engine, &mut stack); // redoes AddSheet -> UndeleteSheet(s1)
        assert_eq!(engine.workbook().live_sheets().count(), 2);

        undo_once(&mut engine, &mut stack);
        undo_once(&mut engine, &mut stack); // undoes Format A1
        assert!(
            !engine
                .workbook()
                .sheet(s0)
                .unwrap()
                .effective_style_by_id(RowId(0), ColId(0))
                .bold
        );

        for i in 0..110 {
            let inv = engine
                .edit_cell_raw(s0, RowId(0), ColId(2), &format!("{i}"))
                .unwrap();
            let op = Op::SetCells {
                sheet: s0,
                edits: Vec::new(),
            };
            stack.record_txn(format!("Edit {i}"), &[op], vec![inv], engine.workbook());
        }
        assert_eq!(stack.undo_len(), MAX_UNDO_ENTRIES);
        assert!(!stack.can_redo());
    }
}
