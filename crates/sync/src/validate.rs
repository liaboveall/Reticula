//! Identity ownership checks and collaborative ID remapping for offline/epoch
//! recovery (`06-sync.md` & `15-m2a.md` 5.1, 5.3).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use reticula_core::id::{
    ColId, IdAllocator, IdRun, RowId, SheetId, id_counter, pack_sheet_id, sheet_id_belongs_to,
};
use reticula_core::ops::{FormatTarget, Op, Workbook};
use reticula_formula::binder::BoundFormula;
use reticula_wire::v2::WireRejectReasonV1;

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
/// when rewriting queue entries onto a new `(epoch, replica)`.
pub(crate) struct RemapState<'a> {
    pub(crate) epoch: u8,
    pub(crate) replica: u32,
    pub(crate) id_alloc: &'a mut IdAllocator,
    pub(crate) next_sheet_counter: &'a mut u8,
    pub(crate) confirmed: &'a Workbook<BoundFormula>,
    pub(crate) sheets: BTreeMap<SheetId, SheetId>,
    pub(crate) rows: BTreeMap<RowId, RowId>,
    pub(crate) cols: BTreeMap<ColId, ColId>,
}

impl RemapState<'_> {
    fn map_sheet(&self, id: SheetId) -> SheetId {
        self.sheets.get(&id).copied().unwrap_or(id)
    }

    fn map_row(&self, id: RowId) -> RowId {
        self.rows.get(&id).copied().unwrap_or(id)
    }

    fn map_col(&self, id: ColId) -> ColId {
        self.cols.get(&id).copied().unwrap_or(id)
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

    pub(crate) fn remap_op(&mut self, op: Op<BoundFormula>) -> Option<Op<BoundFormula>> {
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
