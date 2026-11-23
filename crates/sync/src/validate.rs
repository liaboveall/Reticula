//! Identity ownership checks and collaborative ID remapping for offline/epoch
//! recovery (`06-sync.md` & `15-m2a.md` 5.1, 5.3).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use reticula_core::id::{
    ColId, IdAllocator, IdRun, RowId, SheetId, id_counter, pack_sheet_id, sheet_id_belongs_to,
};
use reticula_core::ops::{CellInput, FormatTarget, FormulaPayload, Op, Workbook};
use reticula_formula::binder::BoundFormula;
use reticula_wire::v2::WireRejectReasonV1;

use crate::types::CommittedPermute;

/// Verifies that every freshly introduced row/column [`IdRun`] and worksheet
/// [`SheetId`] in `ops` belongs exclusively to `(epoch, replica)` and advances
/// identity counters strictly above `cur_max_id_counter`.
///
/// Returns the updated highest ID counter introduced across `ops`.
pub fn validate_txn_ownership(
    epoch: u8,
    replica: u32,
    cur_max_id_counter: u32,
    ops: &[Op<BoundFormula>],
) -> Result<u32, WireRejectReasonV1> {
    let mut seen_max = cur_max_id_counter;
    for op in ops {
        match op {
            Op::InsertRows { ids, .. } | Op::InsertCols { ids, .. } => {
                if !ids.belongs_to(epoch, replica) {
                    return Err(WireRejectReasonV1::InvalidOp);
                }
                let start = id_counter(ids.first_id);
                if start <= seen_max {
                    return Err(WireRejectReasonV1::InvalidOp);
                }
                let Some(end) = ids.max_counter() else {
                    return Err(WireRejectReasonV1::InvalidOp);
                };
                seen_max = end;
            }
            Op::AddSheet { sheet, .. } if !sheet_id_belongs_to(*sheet, epoch, replica) => {
                return Err(WireRejectReasonV1::InvalidOp);
            }
            _ => {}
        }
    }
    Ok(seen_max)
}

/// Computes the maximum ID counter used within `ops` (for replica stream `(epoch, replica)`),
/// used when replaying tail commits during server reload.
#[must_use]
pub fn max_id_counter_in_ops(epoch: u8, replica: u32, ops: &[Op<BoundFormula>]) -> u32 {
    let mut max_c = 0_u32;
    for op in ops {
        if let Op::InsertRows { ids, .. } | Op::InsertCols { ids, .. } = op
            && ids.belongs_to(epoch, replica)
            && let Some(end) = ids.max_counter()
        {
            max_c = max_c.max(end);
        }
    }
    max_c
}

/// Mutable identity remapping tables maintained across pending transactions
/// or branch merges when rewriting operations onto a fresh `(epoch, replica)`.
pub struct RemapState<'a> {
    pub(crate) epoch: u8,
    pub(crate) replica: u32,
    pub(crate) id_alloc: &'a mut IdAllocator,
    pub(crate) next_sheet_counter: &'a mut u8,
    pub(crate) confirmed: &'a Workbook<BoundFormula>,
    pub(crate) sheets: BTreeMap<SheetId, SheetId>,
    pub(crate) rows: BTreeMap<RowId, RowId>,
    pub(crate) cols: BTreeMap<ColId, ColId>,
}

impl<'a> RemapState<'a> {
    /// Creates an identity remapping context targeting `(epoch, replica)` against
    /// the current confirmed `workbook`.
    pub fn new(
        epoch: u8,
        replica: u32,
        id_alloc: &'a mut IdAllocator,
        next_sheet_counter: &'a mut u8,
        confirmed: &'a Workbook<BoundFormula>,
    ) -> Self {
        Self {
            epoch,
            replica,
            id_alloc,
            next_sheet_counter,
            confirmed,
            sheets: BTreeMap::new(),
            rows: BTreeMap::new(),
            cols: BTreeMap::new(),
        }
    }

    /// Maps a worksheet ID through the current remapping table.
    #[must_use]
    pub fn map_sheet(&self, id: SheetId) -> SheetId {
        self.sheets.get(&id).copied().unwrap_or(id)
    }

    /// Maps a row ID through the current remapping table.
    #[must_use]
    pub fn map_row(&self, id: RowId) -> RowId {
        self.rows.get(&id).copied().unwrap_or(id)
    }

    /// Maps a column ID through the current remapping table.
    #[must_use]
    pub fn map_col(&self, id: ColId) -> ColId {
        self.cols.get(&id).copied().unwrap_or(id)
    }

    /// Current remapped worksheet IDs (`draft_id -> target_id`).
    #[must_use]
    pub const fn sheet_map(&self) -> &BTreeMap<SheetId, SheetId> {
        &self.sheets
    }

    /// Current remapped row IDs (`draft_id -> target_id`).
    #[must_use]
    pub const fn row_map(&self) -> &BTreeMap<RowId, RowId> {
        &self.rows
    }

    /// Current remapped column IDs (`draft_id -> target_id`).
    #[must_use]
    pub const fn col_map(&self) -> &BTreeMap<ColId, ColId> {
        &self.cols
    }

    fn map_row_runs(&self, runs: &[IdRun]) -> Vec<IdRun> {
        let mut out = Vec::new();
        for &run in runs {
            for raw in run.iter() {
                let mapped = self.map_row(RowId(raw)).0;
                push_id_coalesced(&mut out, mapped);
            }
        }
        out
    }

    fn map_col_runs(&self, runs: &[IdRun]) -> Vec<IdRun> {
        let mut out = Vec::new();
        for &run in runs {
            for raw in run.iter() {
                let mapped = self.map_col(ColId(raw)).0;
                push_id_coalesced(&mut out, mapped);
            }
        }
        out
    }

    fn alloc_sheet_not_colliding(&mut self) -> Option<SheetId> {
        let mut c = (*self.next_sheet_counter).max(1);
        loop {
            let candidate = pack_sheet_id(self.epoch, self.replica, c)?;
            let taken_in_wb = self.confirmed.sheet(candidate).is_some();
            let taken_in_map = self.sheets.values().any(|&v| v == candidate);
            if !taken_in_wb && !taken_in_map {
                *self.next_sheet_counter = c.saturating_add(1);
                return Some(candidate);
            }
            if c == u8::MAX {
                return None;
            }
            c = c.saturating_add(1);
        }
    }

    /// Remaps all sheet/row/column identities and bound formula references in `op`
    /// onto fresh IDs belonging to `(self.epoch, self.replica)`.
    pub fn remap_op(&mut self, op: Op<BoundFormula>) -> Option<Op<BoundFormula>> {
        match op {
            Op::SetCells { sheet, edits } => {
                let mapped_edits = edits
                    .into_iter()
                    .map(|mut e| {
                        e.row = self.map_row(e.row);
                        e.col = self.map_col(e.col);
                        remap_cell_input(&mut e.input, &self.sheets, &self.rows, &self.cols);
                        e
                    })
                    .collect();
                Some(Op::SetCells {
                    sheet: self.map_sheet(sheet),
                    edits: mapped_edits,
                })
            }
            Op::SetCellsIf { sheet, edits } => {
                let mapped_edits = edits
                    .into_iter()
                    .map(|mut e| {
                        e.row = self.map_row(e.row);
                        e.col = self.map_col(e.col);
                        remap_cell_input(
                            &mut e.expected_input,
                            &self.sheets,
                            &self.rows,
                            &self.cols,
                        );
                        remap_cell_input(&mut e.new_input, &self.sheets, &self.rows, &self.cols);
                        e
                    })
                    .collect();
                Some(Op::SetCellsIf {
                    sheet: self.map_sheet(sheet),
                    edits: mapped_edits,
                })
            }
            Op::ClearRange { sheet, rows, cols } => Some(Op::ClearRange {
                sheet: self.map_sheet(sheet),
                rows: self.map_row_runs(&rows),
                cols: self.map_col_runs(&cols),
            }),
            Op::InsertRows { sheet, after, ids } => {
                let fresh = self.id_alloc.alloc_run(ids.len)?;
                for (old_raw, new_raw) in ids.iter().zip(fresh.iter()) {
                    self.rows.insert(RowId(old_raw), RowId(new_raw));
                }
                Some(Op::InsertRows {
                    sheet: self.map_sheet(sheet),
                    after: after.map(|r| self.map_row(r)),
                    ids: fresh,
                })
            }
            Op::InsertCols { sheet, after, ids } => {
                let fresh = self.id_alloc.alloc_run(ids.len)?;
                for (old_raw, new_raw) in ids.iter().zip(fresh.iter()) {
                    self.cols.insert(ColId(old_raw), ColId(new_raw));
                }
                Some(Op::InsertCols {
                    sheet: self.map_sheet(sheet),
                    after: after.map(|c| self.map_col(c)),
                    ids: fresh,
                })
            }
            Op::DeleteRows { sheet, ids } => Some(Op::DeleteRows {
                sheet: self.map_sheet(sheet),
                ids: self.map_row_runs(&ids),
            }),
            Op::DeleteCols { sheet, ids } => Some(Op::DeleteCols {
                sheet: self.map_sheet(sheet),
                ids: self.map_col_runs(&ids),
            }),
            Op::UndeleteRows { sheet, ids } => Some(Op::UndeleteRows {
                sheet: self.map_sheet(sheet),
                ids: self.map_row_runs(&ids),
            }),
            Op::UndeleteCols { sheet, ids } => Some(Op::UndeleteCols {
                sheet: self.map_sheet(sheet),
                ids: self.map_col_runs(&ids),
            }),
            Op::MoveRows { sheet, ids, after } => Some(Op::MoveRows {
                sheet: self.map_sheet(sheet),
                ids: self.map_row_runs(&ids),
                after: after.map(|r| self.map_row(r)),
            }),
            Op::MoveCols { sheet, ids, after } => Some(Op::MoveCols {
                sheet: self.map_sheet(sheet),
                ids: self.map_col_runs(&ids),
                after: after.map(|c| self.map_col(c)),
            }),
            Op::ResizeRows { sheet, sizes } => Some(Op::ResizeRows {
                sheet: self.map_sheet(sheet),
                sizes: sizes
                    .into_iter()
                    .flat_map(|(r, px)| self.map_row_runs(&[r]).into_iter().map(move |mr| (mr, px)))
                    .collect(),
            }),
            Op::ResizeCols { sheet, sizes } => Some(Op::ResizeCols {
                sheet: self.map_sheet(sheet),
                sizes: sizes
                    .into_iter()
                    .flat_map(|(c, px)| self.map_col_runs(&[c]).into_iter().map(move |mc| (mc, px)))
                    .collect(),
            }),
            Op::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            } => Some(Op::SetFreeze {
                sheet: self.map_sheet(sheet),
                frozen_rows,
                frozen_cols,
            }),
            Op::SetFormat {
                sheet,
                target,
                number_format,
                style,
            } => {
                let mapped_target = match target {
                    FormatTarget::Cells { rows, cols } => FormatTarget::Cells {
                        rows: self.map_row_runs(&rows),
                        cols: self.map_col_runs(&cols),
                    },
                    FormatTarget::Rows(r) => FormatTarget::Rows(self.map_row_runs(&r)),
                    FormatTarget::Cols(c) => FormatTarget::Cols(self.map_col_runs(&c)),
                };
                Some(Op::SetFormat {
                    sheet: self.map_sheet(sheet),
                    target: mapped_target,
                    number_format,
                    style,
                })
            }
            Op::SetBlock {
                sheet,
                rows,
                cols,
                mut columns_data,
            } => {
                for col_vec in &mut columns_data {
                    for cell in col_vec {
                        remap_cell_input(cell, &self.sheets, &self.rows, &self.cols);
                    }
                }
                Some(Op::SetBlock {
                    sheet: self.map_sheet(sheet),
                    rows: self.map_row_runs(&rows),
                    cols: cols.into_iter().map(|c| self.map_col(c)).collect(),
                    columns_data,
                })
            }
            Op::FillFormula {
                sheet,
                rows,
                cols,
                mut template,
            } => {
                template.remap_ids(&self.sheets, &self.rows, &self.cols);
                Some(Op::FillFormula {
                    sheet: self.map_sheet(sheet),
                    rows: self.map_row_runs(&rows),
                    cols: self.map_col_runs(&cols),
                    template,
                })
            }
            Op::AddSheet { sheet, name, after } => {
                let fresh = self.alloc_sheet_not_colliding()?;
                self.sheets.insert(sheet, fresh);
                Some(Op::AddSheet {
                    sheet: fresh,
                    name,
                    after: after.map(|s| self.map_sheet(s)),
                })
            }
            Op::RenameSheet { sheet, name } => Some(Op::RenameSheet {
                sheet: self.map_sheet(sheet),
                name,
            }),
            Op::DeleteSheet { sheet } => Some(Op::DeleteSheet {
                sheet: self.map_sheet(sheet),
            }),
            Op::UndeleteSheet { sheet } => Some(Op::UndeleteSheet {
                sheet: self.map_sheet(sheet),
            }),
            Op::MoveSheet { sheet, after } => Some(Op::MoveSheet {
                sheet: self.map_sheet(sheet),
                after: after.map(|s| self.map_sheet(s)),
            }),
            Op::PermuteContent {
                sheet,
                rows,
                cols,
                row_perm,
            } => Some(Op::PermuteContent {
                sheet: self.map_sheet(sheet),
                rows: rows.into_iter().map(|r| self.map_row(r)).collect(),
                cols: cols.into_iter().map(|c| self.map_col(c)).collect(),
                row_perm,
            }),
            Op::MoveCells {
                src_sheet,
                src_rows,
                src_cols,
                dst_sheet,
                dst_rows,
                dst_cols,
            } => Some(Op::MoveCells {
                src_sheet: self.map_sheet(src_sheet),
                src_rows: src_rows.into_iter().map(|r| self.map_row(r)).collect(),
                src_cols: src_cols.into_iter().map(|c| self.map_col(c)).collect(),
                dst_sheet: self.map_sheet(dst_sheet),
                dst_rows: dst_rows.into_iter().map(|r| self.map_row(r)).collect(),
                dst_cols: dst_cols.into_iter().map(|c| self.map_col(c)).collect(),
            }),
            Op::DefineNamedRange {
                scope,
                name,
                mut formula,
                comment,
                hidden,
            } => {
                formula.remap_ids(&self.sheets, &self.rows, &self.cols);
                Some(Op::DefineNamedRange {
                    scope: scope.map(|s| self.map_sheet(s)),
                    name,
                    formula,
                    comment,
                    hidden,
                })
            }
            Op::DeleteNamedRange { scope, name } => Some(Op::DeleteNamedRange {
                scope: scope.map(|s| self.map_sheet(s)),
                name,
            }),
        }
    }
}

fn remap_cell_input(
    input: &mut reticula_core::ops::CellInput<BoundFormula>,
    sheets: &BTreeMap<SheetId, SheetId>,
    rows: &BTreeMap<RowId, RowId>,
    cols: &BTreeMap<ColId, ColId>,
) {
    if let reticula_core::ops::CellInput::Formula { formula, .. } = input {
        formula.remap_ids(sheets, rows, cols);
    }
}

fn push_id_coalesced(out: &mut Vec<IdRun>, raw_id: u64) {
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

/// Applies E16 row-content permutation transforms to `ops` for every committed
/// permutation in `permutes` that occurred strictly after `base_server_seq`,
/// skipping permutations previously authored locally by the same replica
/// (`author_replica != 0 && p.replica == author_replica && p.client_seq < author_client_seq`).
///
/// Structural operations, `PermuteContent`, `MoveCells`, and named-range ops
/// are left untouched per `06-sync.md` & `17-m3a.md` 6.1.
#[must_use]
pub fn transform_ops_through_permute(
    ops: &[Op<BoundFormula>],
    permutes: &[CommittedPermute],
    base_server_seq: u64,
    author_replica: u32,
    author_client_seq: u64,
) -> Vec<Op<BoundFormula>> {
    let mut current: Vec<Op<BoundFormula>> = ops.to_vec();
    for p in permutes {
        if p.seq <= base_server_seq || p.row_map.is_empty() || p.cols.is_empty() {
            continue;
        }
        if author_replica != 0 && p.replica == author_replica && p.client_seq < author_client_seq {
            continue;
        }
        let mut next = Vec::with_capacity(current.len());
        for op in current {
            transform_op_through_one_permute(op, p, &mut next);
        }
        current = next;
    }
    current
}

fn transform_op_through_one_permute(
    op: Op<BoundFormula>,
    p: &CommittedPermute,
    out: &mut Vec<Op<BoundFormula>>,
) {
    match op {
        Op::SetCells { sheet, mut edits } => {
            if sheet == p.sheet {
                for edit in &mut edits {
                    if p.cols.contains(&edit.col)
                        && let Some(&mapped_row) = p.row_map.get(&edit.row)
                    {
                        edit.row = mapped_row;
                    }
                }
            }
            out.push(Op::SetCells { sheet, edits });
        }
        Op::SetCellsIf { sheet, mut edits } => {
            if sheet == p.sheet {
                for edit in &mut edits {
                    if p.cols.contains(&edit.col)
                        && let Some(&mapped_row) = p.row_map.get(&edit.row)
                    {
                        edit.row = mapped_row;
                    }
                }
            }
            out.push(Op::SetCellsIf { sheet, edits });
        }
        Op::ClearRange { sheet, rows, cols } => {
            if sheet != p.sheet {
                out.push(Op::ClearRange { sheet, rows, cols });
                return;
            }
            let (inside_cols, outside_cols) = partition_col_runs(&cols, &p.cols);
            if inside_cols.is_empty() || !any_row_in_map(&rows, &p.row_map) {
                out.push(Op::ClearRange { sheet, rows, cols });
                return;
            }
            if !outside_cols.is_empty() {
                out.push(Op::ClearRange {
                    sheet,
                    rows: rows.clone(),
                    cols: outside_cols,
                });
            }
            let mapped_rows = map_rows_canonical(&rows, &p.row_map);
            out.push(Op::ClearRange {
                sheet,
                rows: mapped_rows,
                cols: inside_cols,
            });
        }
        Op::SetFormat {
            sheet,
            target: FormatTarget::Cells { rows, cols },
            number_format,
            style,
        } => {
            if sheet != p.sheet {
                out.push(Op::SetFormat {
                    sheet,
                    target: FormatTarget::Cells { rows, cols },
                    number_format,
                    style,
                });
                return;
            }
            let (inside_cols, outside_cols) = partition_col_runs(&cols, &p.cols);
            if inside_cols.is_empty() || !any_row_in_map(&rows, &p.row_map) {
                out.push(Op::SetFormat {
                    sheet,
                    target: FormatTarget::Cells { rows, cols },
                    number_format,
                    style,
                });
                return;
            }
            if !outside_cols.is_empty() {
                out.push(Op::SetFormat {
                    sheet,
                    target: FormatTarget::Cells {
                        rows: rows.clone(),
                        cols: outside_cols,
                    },
                    number_format,
                    style,
                });
            }
            let mapped_rows = map_rows_canonical(&rows, &p.row_map);
            out.push(Op::SetFormat {
                sheet,
                target: FormatTarget::Cells {
                    rows: mapped_rows,
                    cols: inside_cols,
                },
                number_format,
                style,
            });
        }
        Op::SetBlock {
            sheet,
            rows,
            cols,
            columns_data,
        } => {
            if sheet != p.sheet
                || cols.len() != columns_data.len()
                || !cols.iter().any(|c| p.cols.contains(c))
                || !any_row_in_map(&rows, &p.row_map)
            {
                out.push(Op::SetBlock {
                    sheet,
                    rows,
                    cols,
                    columns_data,
                });
                return;
            }
            let mut outside_cols = Vec::new();
            let mut outside_data = Vec::new();
            let mut inside_cols = Vec::new();
            let mut inside_data = Vec::new();
            for (c, col_vec) in cols.into_iter().zip(columns_data) {
                if p.cols.contains(&c) {
                    inside_cols.push(c);
                    inside_data.push(col_vec);
                } else {
                    outside_cols.push(c);
                    outside_data.push(col_vec);
                }
            }
            if !outside_cols.is_empty() {
                out.push(Op::SetBlock {
                    sheet,
                    rows: rows.clone(),
                    cols: outside_cols,
                    columns_data: outside_data,
                });
            }
            let mut mapped_rows = Vec::new();
            for run in &rows {
                for raw_r in run.iter() {
                    let mapped = p.row_map.get(&RowId(raw_r)).map_or(raw_r, |m| m.0);
                    push_id_coalesced(&mut mapped_rows, mapped);
                }
            }
            out.push(Op::SetBlock {
                sheet,
                rows: mapped_rows,
                cols: inside_cols,
                columns_data: inside_data,
            });
        }
        Op::FillFormula {
            sheet,
            rows,
            cols,
            template,
        } => {
            if sheet != p.sheet {
                out.push(Op::FillFormula {
                    sheet,
                    rows,
                    cols,
                    template,
                });
                return;
            }
            let (inside_cols, outside_cols) = partition_col_runs(&cols, &p.cols);
            if inside_cols.is_empty() || !any_row_in_map(&rows, &p.row_map) {
                out.push(Op::FillFormula {
                    sheet,
                    rows,
                    cols,
                    template,
                });
                return;
            }
            if !outside_cols.is_empty() {
                out.push(Op::FillFormula {
                    sheet,
                    rows: rows.clone(),
                    cols: outside_cols,
                    template: template.clone(),
                });
            }
            let mut unaffected_rows = Vec::new();
            let mut affected_pairs: Vec<(RowId, RowId)> = Vec::new();
            for run in &rows {
                for raw_r in run.iter() {
                    let orig = RowId(raw_r);
                    if let Some(&mapped) = p.row_map.get(&orig) {
                        affected_pairs.push((orig, mapped));
                    } else {
                        push_id_coalesced(&mut unaffected_rows, raw_r);
                    }
                }
            }
            if !unaffected_rows.is_empty() {
                out.push(Op::FillFormula {
                    sheet,
                    rows: unaffected_rows,
                    cols: inside_cols.clone(),
                    template: template.clone(),
                });
            }
            if !affected_pairs.is_empty() {
                let mut mapped_rows = Vec::new();
                for &(_, mapped) in &affected_pairs {
                    push_id_coalesced(&mut mapped_rows, mapped.0);
                }
                let cols_vec: Vec<ColId> = inside_cols
                    .iter()
                    .flat_map(|r| r.iter().map(ColId))
                    .collect();
                let mut columns_data = Vec::with_capacity(cols_vec.len());
                for &col in &cols_vec {
                    let mut col_vec = Vec::with_capacity(affected_pairs.len());
                    for &(orig_row, _) in &affected_pairs {
                        col_vec.push(CellInput::Formula {
                            formula: BoundFormula::expand_template(&template, orig_row, col),
                            cached: None,
                        });
                    }
                    columns_data.push(col_vec);
                }
                out.push(Op::SetBlock {
                    sheet,
                    rows: mapped_rows,
                    cols: cols_vec,
                    columns_data,
                });
            }
        }
        other => out.push(other),
    }
}

fn any_row_in_map(rows: &[IdRun], row_map: &BTreeMap<RowId, RowId>) -> bool {
    rows.iter()
        .any(|run| run.iter().any(|r| row_map.contains_key(&RowId(r))))
}

fn partition_col_runs(runs: &[IdRun], p_cols: &BTreeSet<ColId>) -> (Vec<IdRun>, Vec<IdRun>) {
    let mut inside = Vec::new();
    let mut outside = Vec::new();
    for run in runs {
        for raw_c in run.iter() {
            if p_cols.contains(&ColId(raw_c)) {
                push_id_coalesced(&mut inside, raw_c);
            } else {
                push_id_coalesced(&mut outside, raw_c);
            }
        }
    }
    (inside, outside)
}

fn map_rows_canonical(rows: &[IdRun], row_map: &BTreeMap<RowId, RowId>) -> Vec<IdRun> {
    let mut set = BTreeSet::new();
    for run in rows {
        for raw_r in run.iter() {
            let mapped = row_map.get(&RowId(raw_r)).map_or(raw_r, |m| m.0);
            set.insert(mapped);
        }
    }
    let mut out = Vec::new();
    for raw_r in set {
        push_id_coalesced(&mut out, raw_r);
    }
    out
}
