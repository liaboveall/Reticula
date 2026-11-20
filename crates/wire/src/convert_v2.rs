//! Conversions between in-memory `reticula-core` / `reticula-formula` types
//! and frozen [`crate::v2`] wire schemas.

use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{CellEdit, CellInput, ConditionalCellEdit, Op};
use reticula_core::state_digest::{STATE_DIGEST_VERSION, StateDigest};
use reticula_formula::binder::BoundFormula;

use crate::convert_v1::{WireConvertError, check_text_len, runs_from_wire, runs_to_wire};
use crate::v1::{WireCellEditV1, WireCellInputV1, WireFormatPatchV1, WireNumberFormatV1, WireOpV1};
use crate::v2::{WireConditionalCellEditV2, WireOpV2, WireStateDigestV1, WireTxnV2};

impl From<&ConditionalCellEdit<BoundFormula>> for WireConditionalCellEditV2 {
    fn from(e: &ConditionalCellEdit<BoundFormula>) -> Self {
        Self {
            row: e.row.0,
            col: e.col.0,
            expected_input: WireCellInputV1::from(&e.expected_input),
            expected_format: e.expected_format.map(WireNumberFormatV1::from),
            new_input: WireCellInputV1::from(&e.new_input),
            new_format: WireFormatPatchV1::from(e.new_format),
        }
    }
}

impl TryFrom<WireConditionalCellEditV2> for ConditionalCellEdit<BoundFormula> {
    type Error = WireConvertError;

    fn try_from(w: WireConditionalCellEditV2) -> Result<Self, Self::Error> {
        let expected_format = match w.expected_format {
            None => None,
            Some(fmt) => Some(fmt.try_into()?),
        };
        Ok(Self {
            row: RowId(w.row),
            col: ColId(w.col),
            expected_input: w.expected_input.try_into()?,
            expected_format,
            new_input: w.new_input.try_into()?,
            new_format: w.new_format.try_into()?,
        })
    }
}

impl From<&Op<BoundFormula>> for WireOpV2 {
    fn from(op: &Op<BoundFormula>) -> Self {
        match op {
            Op::SetCells { sheet, edits } => Self::SetCells {
                sheet: sheet.0,
                edits: edits.iter().map(WireCellEditV1::from).collect(),
            },
            Op::SetCellsIf { sheet, edits } => Self::SetCellsIf {
                sheet: sheet.0,
                edits: edits.iter().map(WireConditionalCellEditV2::from).collect(),
            },
            Op::ClearRange { sheet, rows, cols } => Self::ClearRange {
                sheet: sheet.0,
                rows: runs_to_wire(rows),
                cols: runs_to_wire(cols),
            },
            Op::InsertRows { sheet, after, ids } => Self::InsertRows {
                sheet: sheet.0,
                after: after.map(|r| r.0),
                ids: (*ids).into(),
            },
            Op::InsertCols { sheet, after, ids } => Self::InsertCols {
                sheet: sheet.0,
                after: after.map(|c| c.0),
                ids: (*ids).into(),
            },
            Op::DeleteRows { sheet, ids } => Self::DeleteRows {
                sheet: sheet.0,
                ids: runs_to_wire(ids),
            },
            Op::DeleteCols { sheet, ids } => Self::DeleteCols {
                sheet: sheet.0,
                ids: runs_to_wire(ids),
            },
            Op::UndeleteRows { sheet, ids } => Self::UndeleteRows {
                sheet: sheet.0,
                ids: runs_to_wire(ids),
            },
            Op::UndeleteCols { sheet, ids } => Self::UndeleteCols {
                sheet: sheet.0,
                ids: runs_to_wire(ids),
            },
            Op::MoveRows { sheet, ids, after } => Self::MoveRows {
                sheet: sheet.0,
                ids: runs_to_wire(ids),
                after: after.map(|r| r.0),
            },
            Op::MoveCols { sheet, ids, after } => Self::MoveCols {
                sheet: sheet.0,
                ids: runs_to_wire(ids),
                after: after.map(|c| c.0),
            },
            Op::ResizeRows { sheet, sizes } => Self::ResizeRows {
                sheet: sheet.0,
                sizes: sizes.iter().map(|&(r, px)| (r.into(), px)).collect(),
            },
            Op::ResizeCols { sheet, sizes } => Self::ResizeCols {
                sheet: sheet.0,
                sizes: sizes.iter().map(|&(c, px)| (c.into(), px)).collect(),
            },
            Op::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            } => Self::SetFreeze {
                sheet: sheet.0,
                frozen_rows: *frozen_rows,
                frozen_cols: *frozen_cols,
            },
            Op::SetFormat {
                sheet,
                target,
                number_format,
                style,
            } => Self::SetFormat {
                sheet: sheet.0,
                target: target.into(),
                number_format: (*number_format).into(),
                style: (*style).into(),
            },
            Op::SetBlock {
                sheet,
                rows,
                cols,
                columns_data,
            } => Self::SetBlock {
                sheet: sheet.0,
                rows: runs_to_wire(rows),
                cols: cols.iter().map(|c| c.0).collect(),
                columns_data: columns_data
                    .iter()
                    .map(|col| col.iter().map(WireCellInputV1::from).collect())
                    .collect(),
            },
            Op::FillFormula {
                sheet,
                rows,
                cols,
                template,
            } => Self::FillFormula {
                sheet: sheet.0,
                rows: runs_to_wire(rows),
                cols: runs_to_wire(cols),
                template: template.into(),
            },
            Op::AddSheet { sheet, name, after } => Self::AddSheet {
                sheet: sheet.0,
                name: name.clone(),
                after: after.map(|s| s.0),
            },
            Op::RenameSheet { sheet, name } => Self::RenameSheet {
                sheet: sheet.0,
                name: name.clone(),
            },
            Op::DeleteSheet { sheet } => Self::DeleteSheet { sheet: sheet.0 },
            Op::UndeleteSheet { sheet } => Self::UndeleteSheet { sheet: sheet.0 },
            Op::MoveSheet { sheet, after } => Self::MoveSheet {
                sheet: sheet.0,
                after: after.map(|s| s.0),
            },
        }
    }
}

impl TryFrom<WireOpV2> for Op<BoundFormula> {
    type Error = WireConvertError;

    fn try_from(w: WireOpV2) -> Result<Self, Self::Error> {
        match w {
            WireOpV2::SetCells { sheet, edits } => {
                let converted: Result<Vec<CellEdit<BoundFormula>>, WireConvertError> =
                    edits.into_iter().map(CellEdit::try_from).collect();
                Ok(Self::SetCells {
                    sheet: SheetId(sheet),
                    edits: converted?,
                })
            }
            WireOpV2::SetCellsIf { sheet, edits } => {
                let converted: Result<Vec<ConditionalCellEdit<BoundFormula>>, WireConvertError> =
                    edits
                        .into_iter()
                        .map(ConditionalCellEdit::try_from)
                        .collect();
                Ok(Self::SetCellsIf {
                    sheet: SheetId(sheet),
                    edits: converted?,
                })
            }
            WireOpV2::ClearRange { sheet, rows, cols } => Ok(Self::ClearRange {
                sheet: SheetId(sheet),
                rows: runs_from_wire(rows)?,
                cols: runs_from_wire(cols)?,
            }),
            WireOpV2::InsertRows { sheet, after, ids } => Ok(Self::InsertRows {
                sheet: SheetId(sheet),
                after: after.map(RowId),
                ids: ids.try_into()?,
            }),
            WireOpV2::InsertCols { sheet, after, ids } => Ok(Self::InsertCols {
                sheet: SheetId(sheet),
                after: after.map(ColId),
                ids: ids.try_into()?,
            }),
            WireOpV2::DeleteRows { sheet, ids } => Ok(Self::DeleteRows {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV2::DeleteCols { sheet, ids } => Ok(Self::DeleteCols {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV2::UndeleteRows { sheet, ids } => Ok(Self::UndeleteRows {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV2::UndeleteCols { sheet, ids } => Ok(Self::UndeleteCols {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV2::MoveRows { sheet, ids, after } => Ok(Self::MoveRows {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
                after: after.map(RowId),
            }),
            WireOpV2::MoveCols { sheet, ids, after } => Ok(Self::MoveCols {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
                after: after.map(ColId),
            }),
            WireOpV2::ResizeRows { sheet, sizes } => {
                let mut out = Vec::with_capacity(sizes.len());
                for (r, px) in sizes {
                    out.push((r.try_into()?, px));
                }
                Ok(Self::ResizeRows {
                    sheet: SheetId(sheet),
                    sizes: out,
                })
            }
            WireOpV2::ResizeCols { sheet, sizes } => {
                let mut out = Vec::with_capacity(sizes.len());
                for (c, px) in sizes {
                    out.push((c.try_into()?, px));
                }
                Ok(Self::ResizeCols {
                    sheet: SheetId(sheet),
                    sizes: out,
                })
            }
            WireOpV2::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            } => Ok(Self::SetFreeze {
                sheet: SheetId(sheet),
                frozen_rows,
                frozen_cols,
            }),
            WireOpV2::SetFormat {
                sheet,
                target,
                number_format,
                style,
            } => Ok(Self::SetFormat {
                sheet: SheetId(sheet),
                target: target.try_into()?,
                number_format: number_format.try_into()?,
                style: style.try_into()?,
            }),
            WireOpV2::SetBlock {
                sheet,
                rows,
                cols,
                columns_data,
            } => {
                let mut out_cols = Vec::with_capacity(columns_data.len());
                for col in columns_data {
                    let converted: Result<Vec<CellInput<BoundFormula>>, WireConvertError> =
                        col.into_iter().map(CellInput::try_from).collect();
                    out_cols.push(converted?);
                }
                Ok(Self::SetBlock {
                    sheet: SheetId(sheet),
                    rows: runs_from_wire(rows)?,
                    cols: cols.into_iter().map(ColId).collect(),
                    columns_data: out_cols,
                })
            }
            WireOpV2::FillFormula {
                sheet,
                rows,
                cols,
                template,
            } => Ok(Self::FillFormula {
                sheet: SheetId(sheet),
                rows: runs_from_wire(rows)?,
                cols: runs_from_wire(cols)?,
                template: template.try_into()?,
            }),
            WireOpV2::AddSheet { sheet, name, after } => {
                check_text_len(&name)?;
                Ok(Self::AddSheet {
                    sheet: SheetId(sheet),
                    name,
                    after: after.map(SheetId),
                })
            }
            WireOpV2::RenameSheet { sheet, name } => {
                check_text_len(&name)?;
                Ok(Self::RenameSheet {
                    sheet: SheetId(sheet),
                    name,
                })
            }
            WireOpV2::DeleteSheet { sheet } => Ok(Self::DeleteSheet {
                sheet: SheetId(sheet),
            }),
            WireOpV2::UndeleteSheet { sheet } => Ok(Self::UndeleteSheet {
                sheet: SheetId(sheet),
            }),
            WireOpV2::MoveSheet { sheet, after } => Ok(Self::MoveSheet {
                sheet: SheetId(sheet),
                after: after.map(SheetId),
            }),
        }
    }
}

impl From<WireOpV1> for WireOpV2 {
    fn from(w: WireOpV1) -> Self {
        match w {
            WireOpV1::SetCells { sheet, edits } => Self::SetCells { sheet, edits },
            WireOpV1::ClearRange { sheet, rows, cols } => Self::ClearRange { sheet, rows, cols },
            WireOpV1::InsertRows { sheet, after, ids } => Self::InsertRows { sheet, after, ids },
            WireOpV1::InsertCols { sheet, after, ids } => Self::InsertCols { sheet, after, ids },
            WireOpV1::DeleteRows { sheet, ids } => Self::DeleteRows { sheet, ids },
            WireOpV1::DeleteCols { sheet, ids } => Self::DeleteCols { sheet, ids },
            WireOpV1::UndeleteRows { sheet, ids } => Self::UndeleteRows { sheet, ids },
            WireOpV1::UndeleteCols { sheet, ids } => Self::UndeleteCols { sheet, ids },
            WireOpV1::MoveRows { sheet, ids, after } => Self::MoveRows { sheet, ids, after },
            WireOpV1::MoveCols { sheet, ids, after } => Self::MoveCols { sheet, ids, after },
            WireOpV1::ResizeRows { sheet, sizes } => Self::ResizeRows { sheet, sizes },
            WireOpV1::ResizeCols { sheet, sizes } => Self::ResizeCols { sheet, sizes },
            WireOpV1::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            } => Self::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            },
            WireOpV1::SetFormat {
                sheet,
                target,
                number_format,
                style,
            } => Self::SetFormat {
                sheet,
                target,
                number_format,
                style,
            },
            WireOpV1::SetBlock {
                sheet,
                rows,
                cols,
                columns_data,
            } => Self::SetBlock {
                sheet,
                rows,
                cols,
                columns_data,
            },
            WireOpV1::FillFormula {
                sheet,
                rows,
                cols,
                template,
            } => Self::FillFormula {
                sheet,
                rows,
                cols,
                template,
            },
            WireOpV1::AddSheet { sheet, name, after } => Self::AddSheet { sheet, name, after },
            WireOpV1::RenameSheet { sheet, name } => Self::RenameSheet { sheet, name },
            WireOpV1::DeleteSheet { sheet } => Self::DeleteSheet { sheet },
            WireOpV1::UndeleteSheet { sheet } => Self::UndeleteSheet { sheet },
            WireOpV1::MoveSheet { sheet, after } => Self::MoveSheet { sheet, after },
        }
    }
}

impl From<StateDigest> for WireStateDigestV1 {
    fn from(d: StateDigest) -> Self {
        Self {
            version: d.version,
            content_lo: d.content_lo,
            content_hi: d.content_hi,
            order: d.order,
        }
    }
}

impl TryFrom<WireStateDigestV1> for StateDigest {
    type Error = WireConvertError;

    fn try_from(w: WireStateDigestV1) -> Result<Self, Self::Error> {
        if w.version != STATE_DIGEST_VERSION {
            return Err(WireConvertError::UnsupportedDigestVersion);
        }
        Ok(Self {
            version: w.version,
            content_lo: w.content_lo,
            content_hi: w.content_hi,
            order: w.order,
        })
    }
}

/// Converts a slice of in-memory operations into a [`WireTxnV2`].
#[must_use]
pub fn ops_to_wire_txn_v2(label: String, ops: &[Op<BoundFormula>]) -> WireTxnV2 {
    WireTxnV2 {
        label,
        ops: ops.iter().map(WireOpV2::from).collect(),
    }
}

/// Validates and converts a [`WireTxnV2`] back into `(label, Vec<Op<BoundFormula>>)`.
pub fn ops_from_wire_txn_v2(
    w: WireTxnV2,
) -> Result<(String, Vec<Op<BoundFormula>>), WireConvertError> {
    check_text_len(&w.label)?;
    let ops: Result<Vec<Op<BoundFormula>>, WireConvertError> =
        w.ops.into_iter().map(Op::try_from).collect();
    Ok((w.label, ops?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;
    use alloc::vec;

    use reticula_core::format::NumberFormat;
    use reticula_core::id::{ColId, RowId, SheetId};
    use reticula_core::ops::{CellInput, ConditionalCellEdit, FormatPatch, Op, ScalarValue};
    use reticula_core::state_digest::StateDigest;

    use super::{WireOpV2, WireStateDigestV1, ops_from_wire_txn_v2, ops_to_wire_txn_v2};

    #[test]
    fn wire_op_v2_and_state_digest_round_trip_cleanly() {
        let cond = Op::SetCellsIf {
            sheet: SheetId(0),
            edits: vec![ConditionalCellEdit {
                row: RowId(1),
                col: ColId(2),
                expected_input: CellInput::Scalar(ScalarValue::Number(5.0)),
                expected_format: Some(NumberFormat::Fixed2),
                new_input: CellInput::Empty,
                new_format: FormatPatch::Clear,
            }],
        };
        let txn = ops_to_wire_txn_v2(String::from("Undo cell"), core::slice::from_ref(&cond));
        assert!(matches!(txn.ops[0], WireOpV2::SetCellsIf { .. }));
        let (lbl, ops_back) = ops_from_wire_txn_v2(txn).unwrap();
        assert_eq!(lbl, "Undo cell");
        assert_eq!(ops_back, vec![cond]);

        let sd = StateDigest::from_parts(0x1234_5678_9ABC_DEF0_FEDC_BA98_7654_3210, 42);
        let wsd = WireStateDigestV1::from(sd);
        assert_eq!(StateDigest::try_from(wsd).unwrap(), sd);
    }
}
