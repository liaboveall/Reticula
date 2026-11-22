//! Validated bidirectional conversions between in-memory
//! `reticula_core` / `reticula_formula` types and [`crate::v3`] wire schemas,
//! plus fallible downgrades to [`crate::v2`] for protocol `v1` clients.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{CellEdit, CellInput, ConditionalCellEdit, Op, ScalarValue};
use reticula_formula::ast::{Literal, MAX_FUNCTION_ARGS, MAX_NESTING_DEPTH};
use reticula_formula::binder::{BoundExpr, BoundFormula, TemplateExpr, TemplateFormula};

use crate::convert_v1::{WireConvertError, check_text_len, runs_from_wire, runs_to_wire};
use crate::v1::{
    WireBoundExprV1, WireBoundFormulaV1, WireCellEditV1, WireCellInputV1, WireFormatPatchV1,
    WireLiteralV1, WireNumberFormatV1, WireScalarValueV1, WireTemplateExprV1,
    WireTemplateFormulaV1,
};
use crate::v2::{
    WireCatchupV1, WireCommittedTxnV2, WireConditionalCellEditV2, WireOpV2, WireSyncFrameV1,
    WireTxnV2,
};
use crate::v3::{
    WireBoundExprV3, WireBoundFormulaV3, WireCatchupV2, WireCellEditV3, WireCellInputV3,
    WireCommittedTxnV3, WireConditionalCellEditV3, WireOpV3, WireSyncFrameV2, WireTemplateExprV3,
    WireTemplateFormulaV3, WireTxnV3,
};

/// Returns `true` when `formula` contains any M3a AST node not representable
/// in wire `v1`/`v2` (`ScopedName`, `SpillRef`, `Opaque`, `PreservedCached`,
/// or `MissingArg`).
#[must_use]
pub fn formula_uses_v3(formula: &BoundFormula) -> bool {
    expr_uses_v3(&formula.root)
}

fn expr_uses_v3(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Reference(_)
        | BoundExpr::Name(_) => false,
        BoundExpr::Paren(inner) | BoundExpr::Unary { expr: inner, .. } => expr_uses_v3(inner),
        BoundExpr::Binary { left, right, .. } => expr_uses_v3(left) || expr_uses_v3(right),
        BoundExpr::Call { args, .. } => args.iter().any(expr_uses_v3),
        BoundExpr::ScopedName { .. }
        | BoundExpr::SpillRef(_)
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => true,
    }
}

// ------------------------------------------------------------ BoundFormula <-> WireBoundFormulaV3

fn bound_expr_to_wire_v3(expr: &BoundExpr) -> Result<WireBoundExprV3, WireConvertError> {
    match expr {
        BoundExpr::Literal(lit) => Ok(WireBoundExprV3::Literal(lit.into())),
        BoundExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => Ok(WireBoundExprV3::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.iter().map(WireLiteralV1::from).collect(),
        }),
        BoundExpr::Reference(r) => Ok(WireBoundExprV3::Reference(r.into())),
        BoundExpr::Name(n) => Ok(WireBoundExprV3::Name(n.clone())),
        BoundExpr::Paren(inner) => Ok(WireBoundExprV3::Paren(Box::new(bound_expr_to_wire_v3(
            inner,
        )?))),
        BoundExpr::Unary { op, expr } => Ok(WireBoundExprV3::Unary {
            op: (*op).into(),
            expr: Box::new(bound_expr_to_wire_v3(expr)?),
        }),
        BoundExpr::Binary { op, left, right } => Ok(WireBoundExprV3::Binary {
            op: (*op).into(),
            left: Box::new(bound_expr_to_wire_v3(left)?),
            right: Box::new(bound_expr_to_wire_v3(right)?),
        }),
        BoundExpr::Call { name, args } => {
            let mut out = Vec::with_capacity(args.len());
            for arg in args {
                out.push(bound_expr_to_wire_v3(arg)?);
            }
            Ok(WireBoundExprV3::Call {
                name: name.clone(),
                args: out,
            })
        }
        BoundExpr::ScopedName {
            sheet,
            missing_sheet,
            name,
        } => Ok(WireBoundExprV3::ScopedName {
            sheet: sheet.map(|s| s.0),
            missing_sheet: missing_sheet.clone(),
            name: name.clone(),
        }),
        BoundExpr::SpillRef(inner) => Ok(WireBoundExprV3::SpillRef(Box::new(
            bound_expr_to_wire_v3(inner)?,
        ))),
        BoundExpr::Opaque { raw } => Ok(WireBoundExprV3::Opaque { raw: raw.clone() }),
        BoundExpr::PreservedCached { raw, cached } => Ok(WireBoundExprV3::PreservedCached {
            raw: raw.clone(),
            cached: cached.into(),
        }),
        BoundExpr::MissingArg => Ok(WireBoundExprV3::MissingArg),
    }
}

fn bound_expr_from_wire_v3(
    w: WireBoundExprV3,
    depth: usize,
) -> Result<BoundExpr, WireConvertError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(WireConvertError::FormulaLimitExceeded);
    }
    let next = depth.saturating_add(1);
    match w {
        WireBoundExprV3::Literal(lit) => Ok(BoundExpr::Literal(lit.try_into()?)),
        WireBoundExprV3::ArrayLiteral {
            rows,
            cols,
            elements,
        } => {
            let elements = convert_array_elements(rows, cols, elements)?;
            Ok(BoundExpr::ArrayLiteral {
                rows,
                cols,
                elements,
            })
        }
        WireBoundExprV3::Reference(r) => Ok(BoundExpr::Reference(r.into())),
        WireBoundExprV3::Name(n) => {
            check_text_len(&n)?;
            Ok(BoundExpr::Name(n))
        }
        WireBoundExprV3::Paren(inner) => Ok(BoundExpr::Paren(Box::new(bound_expr_from_wire_v3(
            *inner, next,
        )?))),
        WireBoundExprV3::Unary { op, expr } => Ok(BoundExpr::Unary {
            op: op.into(),
            expr: Box::new(bound_expr_from_wire_v3(*expr, next)?),
        }),
        WireBoundExprV3::Binary { op, left, right } => Ok(BoundExpr::Binary {
            op: op.into(),
            left: Box::new(bound_expr_from_wire_v3(*left, next)?),
            right: Box::new(bound_expr_from_wire_v3(*right, next)?),
        }),
        WireBoundExprV3::Call { name, args } => {
            if args.len() > MAX_FUNCTION_ARGS {
                return Err(WireConvertError::FormulaLimitExceeded);
            }
            check_text_len(&name)?;
            let mut out = Vec::with_capacity(args.len());
            for arg in args {
                out.push(bound_expr_from_wire_v3(arg, next)?);
            }
            Ok(BoundExpr::Call {
                name: name.to_ascii_uppercase(),
                args: out,
            })
        }
        WireBoundExprV3::ScopedName {
            sheet,
            missing_sheet,
            name,
        } => {
            if let Some(ref ms) = missing_sheet {
                check_text_len(ms)?;
            }
            check_text_len(&name)?;
            Ok(BoundExpr::ScopedName {
                sheet: sheet.map(SheetId),
                missing_sheet,
                name,
            })
        }
        WireBoundExprV3::SpillRef(inner) => Ok(BoundExpr::SpillRef(Box::new(
            bound_expr_from_wire_v3(*inner, next)?,
        ))),
        WireBoundExprV3::Opaque { raw } => {
            check_text_len(&raw)?;
            Ok(BoundExpr::Opaque { raw })
        }
        WireBoundExprV3::PreservedCached { raw, cached } => {
            check_text_len(&raw)?;
            Ok(BoundExpr::PreservedCached {
                raw,
                cached: cached.try_into()?,
            })
        }
        WireBoundExprV3::MissingArg => Ok(BoundExpr::MissingArg),
    }
}

fn convert_array_elements(
    rows: u32,
    cols: u32,
    elements: Vec<WireLiteralV1>,
) -> Result<Vec<Literal>, WireConvertError> {
    if rows == 0 || cols == 0 {
        return Err(WireConvertError::InvalidArrayLiteral);
    }
    let expected = (rows as usize)
        .checked_mul(cols as usize)
        .ok_or(WireConvertError::InvalidArrayLiteral)?;
    if expected != elements.len() {
        return Err(WireConvertError::InvalidArrayLiteral);
    }
    elements.into_iter().map(Literal::try_from).collect()
}

impl TryFrom<&BoundFormula> for WireBoundFormulaV3 {
    type Error = WireConvertError;

    fn try_from(f: &BoundFormula) -> Result<Self, Self::Error> {
        Ok(Self {
            mode: f.mode.into(),
            root: bound_expr_to_wire_v3(&f.root)?,
        })
    }
}

impl TryFrom<WireBoundFormulaV3> for BoundFormula {
    type Error = WireConvertError;

    fn try_from(w: WireBoundFormulaV3) -> Result<Self, Self::Error> {
        Ok(Self {
            mode: w.mode.into(),
            root: bound_expr_from_wire_v3(w.root, 0)?,
        })
    }
}

// ------------------------------------------------------------ TemplateFormula <-> WireTemplateFormulaV3

fn template_expr_to_wire_v3(expr: &TemplateExpr) -> Result<WireTemplateExprV3, WireConvertError> {
    match expr {
        TemplateExpr::Literal(lit) => Ok(WireTemplateExprV3::Literal(lit.into())),
        TemplateExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => Ok(WireTemplateExprV3::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.iter().map(WireLiteralV1::from).collect(),
        }),
        TemplateExpr::Reference(r) => Ok(WireTemplateExprV3::Reference(r.into())),
        TemplateExpr::Name(n) => Ok(WireTemplateExprV3::Name(n.clone())),
        TemplateExpr::Paren(inner) => Ok(WireTemplateExprV3::Paren(Box::new(
            template_expr_to_wire_v3(inner)?,
        ))),
        TemplateExpr::Unary { op, expr } => Ok(WireTemplateExprV3::Unary {
            op: (*op).into(),
            expr: Box::new(template_expr_to_wire_v3(expr)?),
        }),
        TemplateExpr::Binary { op, left, right } => Ok(WireTemplateExprV3::Binary {
            op: (*op).into(),
            left: Box::new(template_expr_to_wire_v3(left)?),
            right: Box::new(template_expr_to_wire_v3(right)?),
        }),
        TemplateExpr::Call { name, args } => {
            let mut out = Vec::with_capacity(args.len());
            for arg in args {
                out.push(template_expr_to_wire_v3(arg)?);
            }
            Ok(WireTemplateExprV3::Call {
                name: name.clone(),
                args: out,
            })
        }
        TemplateExpr::ScopedName {
            sheet,
            missing_sheet,
            name,
        } => Ok(WireTemplateExprV3::ScopedName {
            sheet: sheet.map(|s| s.0),
            missing_sheet: missing_sheet.clone(),
            name: name.clone(),
        }),
        TemplateExpr::SpillRef(inner) => Ok(WireTemplateExprV3::SpillRef(Box::new(
            template_expr_to_wire_v3(inner)?,
        ))),
        TemplateExpr::Opaque { raw } => Ok(WireTemplateExprV3::Opaque { raw: raw.clone() }),
        TemplateExpr::PreservedCached { raw, cached } => Ok(WireTemplateExprV3::PreservedCached {
            raw: raw.clone(),
            cached: cached.into(),
        }),
        TemplateExpr::MissingArg => Ok(WireTemplateExprV3::MissingArg),
    }
}

fn template_expr_from_wire_v3(
    w: WireTemplateExprV3,
    depth: usize,
) -> Result<TemplateExpr, WireConvertError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(WireConvertError::FormulaLimitExceeded);
    }
    let next = depth.saturating_add(1);
    match w {
        WireTemplateExprV3::Literal(lit) => Ok(TemplateExpr::Literal(lit.try_into()?)),
        WireTemplateExprV3::ArrayLiteral {
            rows,
            cols,
            elements,
        } => {
            let elements = convert_array_elements(rows, cols, elements)?;
            Ok(TemplateExpr::ArrayLiteral {
                rows,
                cols,
                elements,
            })
        }
        WireTemplateExprV3::Reference(r) => Ok(TemplateExpr::Reference(r.into())),
        WireTemplateExprV3::Name(n) => {
            check_text_len(&n)?;
            Ok(TemplateExpr::Name(n))
        }
        WireTemplateExprV3::Paren(inner) => Ok(TemplateExpr::Paren(Box::new(
            template_expr_from_wire_v3(*inner, next)?,
        ))),
        WireTemplateExprV3::Unary { op, expr } => Ok(TemplateExpr::Unary {
            op: op.into(),
            expr: Box::new(template_expr_from_wire_v3(*expr, next)?),
        }),
        WireTemplateExprV3::Binary { op, left, right } => Ok(TemplateExpr::Binary {
            op: op.into(),
            left: Box::new(template_expr_from_wire_v3(*left, next)?),
            right: Box::new(template_expr_from_wire_v3(*right, next)?),
        }),
        WireTemplateExprV3::Call { name, args } => {
            if args.len() > MAX_FUNCTION_ARGS {
                return Err(WireConvertError::FormulaLimitExceeded);
            }
            check_text_len(&name)?;
            let mut out = Vec::with_capacity(args.len());
            for arg in args {
                out.push(template_expr_from_wire_v3(arg, next)?);
            }
            Ok(TemplateExpr::Call {
                name: name.to_ascii_uppercase(),
                args: out,
            })
        }
        WireTemplateExprV3::ScopedName {
            sheet,
            missing_sheet,
            name,
        } => {
            if let Some(ref ms) = missing_sheet {
                check_text_len(ms)?;
            }
            check_text_len(&name)?;
            Ok(TemplateExpr::ScopedName {
                sheet: sheet.map(SheetId),
                missing_sheet,
                name,
            })
        }
        WireTemplateExprV3::SpillRef(inner) => Ok(TemplateExpr::SpillRef(Box::new(
            template_expr_from_wire_v3(*inner, next)?,
        ))),
        WireTemplateExprV3::Opaque { raw } => {
            check_text_len(&raw)?;
            Ok(TemplateExpr::Opaque { raw })
        }
        WireTemplateExprV3::PreservedCached { raw, cached } => {
            check_text_len(&raw)?;
            Ok(TemplateExpr::PreservedCached {
                raw,
                cached: cached.try_into()?,
            })
        }
        WireTemplateExprV3::MissingArg => Ok(TemplateExpr::MissingArg),
    }
}

impl TryFrom<&TemplateFormula> for WireTemplateFormulaV3 {
    type Error = WireConvertError;

    fn try_from(t: &TemplateFormula) -> Result<Self, Self::Error> {
        Ok(Self {
            mode: t.mode.into(),
            root: template_expr_to_wire_v3(&t.root)?,
        })
    }
}

impl TryFrom<WireTemplateFormulaV3> for TemplateFormula {
    type Error = WireConvertError;

    fn try_from(w: WireTemplateFormulaV3) -> Result<Self, Self::Error> {
        Ok(Self {
            mode: w.mode.into(),
            root: template_expr_from_wire_v3(w.root, 0)?,
        })
    }
}

// ------------------------------------------------------------ CellInput & Edits

impl TryFrom<&CellInput<BoundFormula>> for WireCellInputV3 {
    type Error = WireConvertError;

    fn try_from(inp: &CellInput<BoundFormula>) -> Result<Self, Self::Error> {
        match inp {
            CellInput::Empty => Ok(Self::Empty),
            CellInput::Scalar(s) => Ok(Self::Scalar(s.into())),
            CellInput::Formula { formula, cached } => Ok(Self::Formula {
                formula: WireBoundFormulaV3::try_from(formula)?,
                cached: cached.as_ref().map(WireScalarValueV1::from),
            }),
        }
    }
}

impl TryFrom<WireCellInputV3> for CellInput<BoundFormula> {
    type Error = WireConvertError;

    fn try_from(w: WireCellInputV3) -> Result<Self, Self::Error> {
        match w {
            WireCellInputV3::Empty => Ok(Self::Empty),
            WireCellInputV3::Scalar(s) => Ok(Self::Scalar(s.try_into()?)),
            WireCellInputV3::Formula { formula, cached } => Ok(Self::Formula {
                formula: formula.try_into()?,
                cached: match cached {
                    Some(c) => Some(ScalarValue::try_from(c)?),
                    None => None,
                },
            }),
        }
    }
}

impl TryFrom<&CellEdit<BoundFormula>> for WireCellEditV3 {
    type Error = WireConvertError;

    fn try_from(e: &CellEdit<BoundFormula>) -> Result<Self, Self::Error> {
        Ok(Self {
            row: e.row.0,
            col: e.col.0,
            input: (&e.input).try_into()?,
            format: e.format.into(),
        })
    }
}

impl TryFrom<WireCellEditV3> for CellEdit<BoundFormula> {
    type Error = WireConvertError;

    fn try_from(w: WireCellEditV3) -> Result<Self, Self::Error> {
        Ok(Self {
            row: RowId(w.row),
            col: ColId(w.col),
            input: w.input.try_into()?,
            format: w.format.try_into()?,
        })
    }
}

impl TryFrom<&ConditionalCellEdit<BoundFormula>> for WireConditionalCellEditV3 {
    type Error = WireConvertError;

    fn try_from(e: &ConditionalCellEdit<BoundFormula>) -> Result<Self, Self::Error> {
        Ok(Self {
            row: e.row.0,
            col: e.col.0,
            expected_input: (&e.expected_input).try_into()?,
            expected_format: e.expected_format.map(WireNumberFormatV1::from),
            new_input: (&e.new_input).try_into()?,
            new_format: WireFormatPatchV1::from(e.new_format),
        })
    }
}

impl TryFrom<WireConditionalCellEditV3> for ConditionalCellEdit<BoundFormula> {
    type Error = WireConvertError;

    fn try_from(w: WireConditionalCellEditV3) -> Result<Self, Self::Error> {
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

// ------------------------------------------------------------ Op<BoundFormula> <-> WireOpV3

impl TryFrom<&Op<BoundFormula>> for WireOpV3 {
    type Error = WireConvertError;

    fn try_from(op: &Op<BoundFormula>) -> Result<Self, Self::Error> {
        let converted = match op {
            Op::SetCells { sheet, edits } => Self::SetCells {
                sheet: sheet.0,
                edits: edits
                    .iter()
                    .map(WireCellEditV3::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
            },
            Op::SetCellsIf { sheet, edits } => Self::SetCellsIf {
                sheet: sheet.0,
                edits: edits
                    .iter()
                    .map(WireConditionalCellEditV3::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
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
                    .map(|col| {
                        col.iter()
                            .map(WireCellInputV3::try_from)
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .collect::<Result<Vec<_>, _>>()?,
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
                template: template.try_into()?,
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
            Op::PermuteContent {
                sheet,
                rows,
                cols,
                row_perm,
            } => Self::PermuteContent {
                sheet: sheet.0,
                rows: rows.iter().map(|r| r.0).collect(),
                cols: cols.iter().map(|c| c.0).collect(),
                row_perm: row_perm.clone(),
            },
            Op::MoveCells {
                src_sheet,
                src_rows,
                src_cols,
                dst_sheet,
                dst_rows,
                dst_cols,
            } => Self::MoveCells {
                src_sheet: src_sheet.0,
                src_rows: src_rows.iter().map(|r| r.0).collect(),
                src_cols: src_cols.iter().map(|c| c.0).collect(),
                dst_sheet: dst_sheet.0,
                dst_rows: dst_rows.iter().map(|r| r.0).collect(),
                dst_cols: dst_cols.iter().map(|c| c.0).collect(),
            },
            Op::DefineNamedRange {
                scope,
                name,
                formula,
                comment,
                hidden,
            } => Self::DefineNamedRange {
                scope: scope.map(|s| s.0),
                name: name.clone(),
                formula: WireBoundFormulaV3::try_from(formula)?,
                comment: comment.clone(),
                hidden: *hidden,
            },
            Op::DeleteNamedRange { scope, name } => Self::DeleteNamedRange {
                scope: scope.map(|s| s.0),
                name: name.clone(),
            },
        };
        Ok(converted)
    }
}

impl TryFrom<WireOpV3> for Op<BoundFormula> {
    type Error = WireConvertError;

    fn try_from(w: WireOpV3) -> Result<Self, Self::Error> {
        match w {
            WireOpV3::SetCells { sheet, edits } => Ok(Self::SetCells {
                sheet: SheetId(sheet),
                edits: edits
                    .into_iter()
                    .map(CellEdit::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            WireOpV3::SetCellsIf { sheet, edits } => Ok(Self::SetCellsIf {
                sheet: SheetId(sheet),
                edits: edits
                    .into_iter()
                    .map(ConditionalCellEdit::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            WireOpV3::ClearRange { sheet, rows, cols } => Ok(Self::ClearRange {
                sheet: SheetId(sheet),
                rows: runs_from_wire(rows)?,
                cols: runs_from_wire(cols)?,
            }),
            WireOpV3::InsertRows { sheet, after, ids } => Ok(Self::InsertRows {
                sheet: SheetId(sheet),
                after: after.map(RowId),
                ids: ids.try_into()?,
            }),
            WireOpV3::InsertCols { sheet, after, ids } => Ok(Self::InsertCols {
                sheet: SheetId(sheet),
                after: after.map(ColId),
                ids: ids.try_into()?,
            }),
            WireOpV3::DeleteRows { sheet, ids } => Ok(Self::DeleteRows {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV3::DeleteCols { sheet, ids } => Ok(Self::DeleteCols {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV3::UndeleteRows { sheet, ids } => Ok(Self::UndeleteRows {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV3::UndeleteCols { sheet, ids } => Ok(Self::UndeleteCols {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV3::MoveRows { sheet, ids, after } => Ok(Self::MoveRows {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
                after: after.map(RowId),
            }),
            WireOpV3::MoveCols { sheet, ids, after } => Ok(Self::MoveCols {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
                after: after.map(ColId),
            }),
            WireOpV3::ResizeRows { sheet, sizes } => {
                let mut out = Vec::with_capacity(sizes.len());
                for (r, px) in sizes {
                    out.push((r.try_into()?, px));
                }
                Ok(Self::ResizeRows {
                    sheet: SheetId(sheet),
                    sizes: out,
                })
            }
            WireOpV3::ResizeCols { sheet, sizes } => {
                let mut out = Vec::with_capacity(sizes.len());
                for (c, px) in sizes {
                    out.push((c.try_into()?, px));
                }
                Ok(Self::ResizeCols {
                    sheet: SheetId(sheet),
                    sizes: out,
                })
            }
            WireOpV3::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            } => Ok(Self::SetFreeze {
                sheet: SheetId(sheet),
                frozen_rows,
                frozen_cols,
            }),
            WireOpV3::SetFormat {
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
            WireOpV3::SetBlock {
                sheet,
                rows,
                cols,
                columns_data,
            } => {
                let mut out_cols = Vec::with_capacity(columns_data.len());
                for col in columns_data {
                    out_cols.push(
                        col.into_iter()
                            .map(CellInput::try_from)
                            .collect::<Result<Vec<_>, _>>()?,
                    );
                }
                Ok(Self::SetBlock {
                    sheet: SheetId(sheet),
                    rows: runs_from_wire(rows)?,
                    cols: cols.into_iter().map(ColId).collect(),
                    columns_data: out_cols,
                })
            }
            WireOpV3::FillFormula {
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
            WireOpV3::AddSheet { sheet, name, after } => {
                check_text_len(&name)?;
                Ok(Self::AddSheet {
                    sheet: SheetId(sheet),
                    name,
                    after: after.map(SheetId),
                })
            }
            WireOpV3::RenameSheet { sheet, name } => {
                check_text_len(&name)?;
                Ok(Self::RenameSheet {
                    sheet: SheetId(sheet),
                    name,
                })
            }
            WireOpV3::DeleteSheet { sheet } => Ok(Self::DeleteSheet {
                sheet: SheetId(sheet),
            }),
            WireOpV3::UndeleteSheet { sheet } => Ok(Self::UndeleteSheet {
                sheet: SheetId(sheet),
            }),
            WireOpV3::MoveSheet { sheet, after } => Ok(Self::MoveSheet {
                sheet: SheetId(sheet),
                after: after.map(SheetId),
            }),
            WireOpV3::PermuteContent {
                sheet,
                rows,
                cols,
                row_perm,
            } => Ok(Self::PermuteContent {
                sheet: SheetId(sheet),
                rows: rows.into_iter().map(RowId).collect(),
                cols: cols.into_iter().map(ColId).collect(),
                row_perm,
            }),
            WireOpV3::MoveCells {
                src_sheet,
                src_rows,
                src_cols,
                dst_sheet,
                dst_rows,
                dst_cols,
            } => Ok(Self::MoveCells {
                src_sheet: SheetId(src_sheet),
                src_rows: src_rows.into_iter().map(RowId).collect(),
                src_cols: src_cols.into_iter().map(ColId).collect(),
                dst_sheet: SheetId(dst_sheet),
                dst_rows: dst_rows.into_iter().map(RowId).collect(),
                dst_cols: dst_cols.into_iter().map(ColId).collect(),
            }),
            WireOpV3::DefineNamedRange {
                scope,
                name,
                formula,
                comment,
                hidden,
            } => {
                check_text_len(&name)?;
                if let Some(ref c) = comment {
                    check_text_len(c)?;
                }
                Ok(Self::DefineNamedRange {
                    scope: scope.map(SheetId),
                    name,
                    formula: formula.try_into()?,
                    comment,
                    hidden,
                })
            }
            WireOpV3::DeleteNamedRange { scope, name } => {
                check_text_len(&name)?;
                Ok(Self::DeleteNamedRange {
                    scope: scope.map(SheetId),
                    name,
                })
            }
        }
    }
}

/// Converts a slice of in-memory operations into a [`WireTxnV3`].
pub fn ops_to_wire_txn_v3(
    label: String,
    ops: &[Op<BoundFormula>],
) -> Result<WireTxnV3, WireConvertError> {
    let converted: Result<Vec<WireOpV3>, WireConvertError> =
        ops.iter().map(WireOpV3::try_from).collect();
    Ok(WireTxnV3 {
        label,
        ops: converted?,
    })
}

/// Validates and converts a [`WireTxnV3`] back into `(label, Vec<Op<BoundFormula>>)`.
pub fn ops_from_wire_txn_v3(
    w: WireTxnV3,
) -> Result<(String, Vec<Op<BoundFormula>>), WireConvertError> {
    check_text_len(&w.label)?;
    let ops: Result<Vec<Op<BoundFormula>>, WireConvertError> =
        w.ops.into_iter().map(Op::try_from).collect();
    Ok((w.label, ops?))
}

// ------------------------------------------------------------ Fallible V3 -> V2/V1 downgrades

fn bound_expr_to_v1(expr: &WireBoundExprV3) -> Result<WireBoundExprV1, WireConvertError> {
    match expr {
        WireBoundExprV3::Literal(l) => Ok(WireBoundExprV1::Literal(l.clone())),
        WireBoundExprV3::ArrayLiteral {
            rows,
            cols,
            elements,
        } => Ok(WireBoundExprV1::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.clone(),
        }),
        WireBoundExprV3::Reference(r) => Ok(WireBoundExprV1::Reference(r.clone())),
        WireBoundExprV3::Name(n) => Ok(WireBoundExprV1::Name(n.clone())),
        WireBoundExprV3::Paren(p) => Ok(WireBoundExprV1::Paren(Box::new(bound_expr_to_v1(p)?))),
        WireBoundExprV3::Unary { op, expr } => Ok(WireBoundExprV1::Unary {
            op: *op,
            expr: Box::new(bound_expr_to_v1(expr)?),
        }),
        WireBoundExprV3::Binary { op, left, right } => Ok(WireBoundExprV1::Binary {
            op: *op,
            left: Box::new(bound_expr_to_v1(left)?),
            right: Box::new(bound_expr_to_v1(right)?),
        }),
        WireBoundExprV3::Call { name, args } => Ok(WireBoundExprV1::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(bound_expr_to_v1)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        WireBoundExprV3::ScopedName { .. }
        | WireBoundExprV3::SpillRef(_)
        | WireBoundExprV3::Opaque { .. }
        | WireBoundExprV3::PreservedCached { .. }
        | WireBoundExprV3::MissingArg => Err(WireConvertError::UnsupportedInV2),
    }
}

fn bound_formula_to_v1(f: &WireBoundFormulaV3) -> Result<WireBoundFormulaV1, WireConvertError> {
    Ok(WireBoundFormulaV1 {
        mode: f.mode,
        root: bound_expr_to_v1(&f.root)?,
    })
}

fn template_expr_to_v1(expr: &WireTemplateExprV3) -> Result<WireTemplateExprV1, WireConvertError> {
    match expr {
        WireTemplateExprV3::Literal(l) => Ok(WireTemplateExprV1::Literal(l.clone())),
        WireTemplateExprV3::ArrayLiteral {
            rows,
            cols,
            elements,
        } => Ok(WireTemplateExprV1::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.clone(),
        }),
        WireTemplateExprV3::Reference(r) => Ok(WireTemplateExprV1::Reference(r.clone())),
        WireTemplateExprV3::Name(n) => Ok(WireTemplateExprV1::Name(n.clone())),
        WireTemplateExprV3::Paren(p) => {
            Ok(WireTemplateExprV1::Paren(Box::new(template_expr_to_v1(p)?)))
        }
        WireTemplateExprV3::Unary { op, expr } => Ok(WireTemplateExprV1::Unary {
            op: *op,
            expr: Box::new(template_expr_to_v1(expr)?),
        }),
        WireTemplateExprV3::Binary { op, left, right } => Ok(WireTemplateExprV1::Binary {
            op: *op,
            left: Box::new(template_expr_to_v1(left)?),
            right: Box::new(template_expr_to_v1(right)?),
        }),
        WireTemplateExprV3::Call { name, args } => Ok(WireTemplateExprV1::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(template_expr_to_v1)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        WireTemplateExprV3::ScopedName { .. }
        | WireTemplateExprV3::SpillRef(_)
        | WireTemplateExprV3::Opaque { .. }
        | WireTemplateExprV3::PreservedCached { .. }
        | WireTemplateExprV3::MissingArg => Err(WireConvertError::UnsupportedInV2),
    }
}

fn template_formula_to_v1(
    t: &WireTemplateFormulaV3,
) -> Result<WireTemplateFormulaV1, WireConvertError> {
    Ok(WireTemplateFormulaV1 {
        mode: t.mode,
        root: template_expr_to_v1(&t.root)?,
    })
}

fn cell_input_to_v1(inp: &WireCellInputV3) -> Result<WireCellInputV1, WireConvertError> {
    match inp {
        WireCellInputV3::Empty => Ok(WireCellInputV1::Empty),
        WireCellInputV3::Scalar(s) => Ok(WireCellInputV1::Scalar(s.clone())),
        WireCellInputV3::Formula { formula, cached } => Ok(WireCellInputV1::Formula {
            formula: bound_formula_to_v1(formula)?,
            cached: cached.clone(),
        }),
    }
}

fn cell_edit_to_v1(e: &WireCellEditV3) -> Result<WireCellEditV1, WireConvertError> {
    Ok(WireCellEditV1 {
        row: e.row,
        col: e.col,
        input: cell_input_to_v1(&e.input)?,
        format: e.format,
    })
}

fn cond_edit_to_v2(
    e: &WireConditionalCellEditV3,
) -> Result<WireConditionalCellEditV2, WireConvertError> {
    Ok(WireConditionalCellEditV2 {
        row: e.row,
        col: e.col,
        expected_input: cell_input_to_v1(&e.expected_input)?,
        expected_format: e.expected_format,
        new_input: cell_input_to_v1(&e.new_input)?,
        new_format: e.new_format,
    })
}

impl TryFrom<&WireOpV3> for WireOpV2 {
    type Error = WireConvertError;

    fn try_from(op: &WireOpV3) -> Result<Self, Self::Error> {
        match op {
            WireOpV3::SetCells { sheet, edits } => Ok(Self::SetCells {
                sheet: *sheet,
                edits: edits
                    .iter()
                    .map(cell_edit_to_v1)
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            WireOpV3::SetCellsIf { sheet, edits } => Ok(Self::SetCellsIf {
                sheet: *sheet,
                edits: edits
                    .iter()
                    .map(cond_edit_to_v2)
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            WireOpV3::ClearRange { sheet, rows, cols } => Ok(Self::ClearRange {
                sheet: *sheet,
                rows: rows.clone(),
                cols: cols.clone(),
            }),
            WireOpV3::InsertRows { sheet, after, ids } => Ok(Self::InsertRows {
                sheet: *sheet,
                after: *after,
                ids: *ids,
            }),
            WireOpV3::InsertCols { sheet, after, ids } => Ok(Self::InsertCols {
                sheet: *sheet,
                after: *after,
                ids: *ids,
            }),
            WireOpV3::DeleteRows { sheet, ids } => Ok(Self::DeleteRows {
                sheet: *sheet,
                ids: ids.clone(),
            }),
            WireOpV3::DeleteCols { sheet, ids } => Ok(Self::DeleteCols {
                sheet: *sheet,
                ids: ids.clone(),
            }),
            WireOpV3::UndeleteRows { sheet, ids } => Ok(Self::UndeleteRows {
                sheet: *sheet,
                ids: ids.clone(),
            }),
            WireOpV3::UndeleteCols { sheet, ids } => Ok(Self::UndeleteCols {
                sheet: *sheet,
                ids: ids.clone(),
            }),
            WireOpV3::MoveRows { sheet, ids, after } => Ok(Self::MoveRows {
                sheet: *sheet,
                ids: ids.clone(),
                after: *after,
            }),
            WireOpV3::MoveCols { sheet, ids, after } => Ok(Self::MoveCols {
                sheet: *sheet,
                ids: ids.clone(),
                after: *after,
            }),
            WireOpV3::ResizeRows { sheet, sizes } => Ok(Self::ResizeRows {
                sheet: *sheet,
                sizes: sizes.clone(),
            }),
            WireOpV3::ResizeCols { sheet, sizes } => Ok(Self::ResizeCols {
                sheet: *sheet,
                sizes: sizes.clone(),
            }),
            WireOpV3::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            } => Ok(Self::SetFreeze {
                sheet: *sheet,
                frozen_rows: *frozen_rows,
                frozen_cols: *frozen_cols,
            }),
            WireOpV3::SetFormat {
                sheet,
                target,
                number_format,
                style,
            } => Ok(Self::SetFormat {
                sheet: *sheet,
                target: target.clone(),
                number_format: *number_format,
                style: *style,
            }),
            WireOpV3::SetBlock {
                sheet,
                rows,
                cols,
                columns_data,
            } => Ok(Self::SetBlock {
                sheet: *sheet,
                rows: rows.clone(),
                cols: cols.clone(),
                columns_data: columns_data
                    .iter()
                    .map(|col| {
                        col.iter()
                            .map(cell_input_to_v1)
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            WireOpV3::FillFormula {
                sheet,
                rows,
                cols,
                template,
            } => Ok(Self::FillFormula {
                sheet: *sheet,
                rows: rows.clone(),
                cols: cols.clone(),
                template: template_formula_to_v1(template)?,
            }),
            WireOpV3::AddSheet { sheet, name, after } => Ok(Self::AddSheet {
                sheet: *sheet,
                name: name.clone(),
                after: *after,
            }),
            WireOpV3::RenameSheet { sheet, name } => Ok(Self::RenameSheet {
                sheet: *sheet,
                name: name.clone(),
            }),
            WireOpV3::DeleteSheet { sheet } => Ok(Self::DeleteSheet { sheet: *sheet }),
            WireOpV3::UndeleteSheet { sheet } => Ok(Self::UndeleteSheet { sheet: *sheet }),
            WireOpV3::MoveSheet { sheet, after } => Ok(Self::MoveSheet {
                sheet: *sheet,
                after: *after,
            }),
            WireOpV3::PermuteContent { .. }
            | WireOpV3::MoveCells { .. }
            | WireOpV3::DefineNamedRange { .. }
            | WireOpV3::DeleteNamedRange { .. } => Err(WireConvertError::UnsupportedInV2),
        }
    }
}

impl TryFrom<&WireTxnV3> for WireTxnV2 {
    type Error = WireConvertError;

    fn try_from(t: &WireTxnV3) -> Result<Self, Self::Error> {
        Ok(Self {
            label: t.label.clone(),
            ops: t
                .ops
                .iter()
                .map(WireOpV2::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

impl TryFrom<&WireCommittedTxnV3> for WireCommittedTxnV2 {
    type Error = WireConvertError;

    fn try_from(c: &WireCommittedTxnV3) -> Result<Self, Self::Error> {
        Ok(Self {
            seq: c.seq,
            replica: c.replica,
            client_seq: c.client_seq,
            user_id: c.user_id,
            timestamp_ms: c.timestamp_ms,
            txn: WireTxnV2::try_from(&c.txn)?,
        })
    }
}

impl TryFrom<&WireCatchupV2> for WireCatchupV1 {
    type Error = WireConvertError;

    fn try_from(c: &WireCatchupV2) -> Result<Self, Self::Error> {
        match c {
            WireCatchupV2::Txns { from_seq, txns } => Ok(Self::Txns {
                from_seq: *from_seq,
                txns: txns
                    .iter()
                    .map(WireCommittedTxnV2::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            WireCatchupV2::SnapshotRequired { snapshot_seq, url } => Ok(Self::SnapshotRequired {
                snapshot_seq: *snapshot_seq,
                url: url.clone(),
            }),
        }
    }
}

impl TryFrom<&WireSyncFrameV2> for WireSyncFrameV1 {
    type Error = WireConvertError;

    fn try_from(f: &WireSyncFrameV2) -> Result<Self, Self::Error> {
        match f {
            WireSyncFrameV2::Hello {
                protocol_version,
                doc_id,
                branch_id,
                replica,
                last_applied_seq,
                known_epoch,
            } => Ok(Self::Hello {
                protocol_version: *protocol_version,
                doc_id: *doc_id,
                branch_id: *branch_id,
                replica: *replica,
                last_applied_seq: *last_applied_seq,
                known_epoch: *known_epoch,
            }),
            WireSyncFrameV2::Welcome {
                epoch,
                head_seq,
                replica,
                last_client_seq,
                max_id_counter,
                catchup,
            } => Ok(Self::Welcome {
                epoch: *epoch,
                head_seq: *head_seq,
                replica: *replica,
                last_client_seq: *last_client_seq,
                max_id_counter: *max_id_counter,
                catchup: WireCatchupV1::try_from(catchup)?,
            }),
            WireSyncFrameV2::Push { txns } => Ok(Self::Push {
                txns: txns
                    .iter()
                    .map(|p| {
                        Ok(crate::v2::WirePendingTxnV2 {
                            replica: p.replica,
                            client_seq: p.client_seq,
                            base_server_seq: p.base_server_seq,
                            timestamp_ms: p.timestamp_ms,
                            txn: WireTxnV2::try_from(&p.txn)?,
                        })
                    })
                    .collect::<Result<Vec<_>, WireConvertError>>()?,
            }),
            WireSyncFrameV2::Ack {
                replica,
                client_seq,
                seq,
                timestamp_ms,
            } => Ok(Self::Ack {
                replica: *replica,
                client_seq: *client_seq,
                seq: *seq,
                timestamp_ms: *timestamp_ms,
            }),
            WireSyncFrameV2::Reject { client_seq, reason } => Ok(Self::Reject {
                client_seq: *client_seq,
                reason: *reason,
            }),
            WireSyncFrameV2::Commit {
                seq,
                replica,
                client_seq,
                user_id,
                timestamp_ms,
                txn,
            } => Ok(Self::Commit {
                seq: *seq,
                replica: *replica,
                client_seq: *client_seq,
                user_id: *user_id,
                timestamp_ms: *timestamp_ms,
                txn: WireTxnV2::try_from(txn)?,
            }),
            WireSyncFrameV2::Digest { seq, state_digest } => Ok(Self::Digest {
                seq: *seq,
                state_digest: *state_digest,
            }),
            WireSyncFrameV2::Presence(p) => Ok(Self::Presence(p.clone())),
            WireSyncFrameV2::Ping(n) => Ok(Self::Ping(*n)),
            WireSyncFrameV2::Pong(n) => Ok(Self::Pong(*n)),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used
)]
mod tests {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec;

    use reticula_core::id::{ColId, RowId, SheetId};
    use reticula_core::ops::{CellEdit, CellInput, FormatPatch, FormulaMode, Op, ScalarValue};
    use reticula_formula::ast::Literal;
    use reticula_formula::binder::{
        AxisTarget, BoundExpr, BoundFormula, BoundRef, TemplateExpr, TemplateFormula, TemplateRef,
    };

    use super::{formula_uses_v3, ops_from_wire_txn_v3, ops_to_wire_txn_v3};
    use crate::convert_v1::WireConvertError;
    use crate::v2::WireOpV2;
    use crate::v3::{WireOpV3, WireSyncFrameV2};

    #[test]
    fn round_trips_all_v3_ops_and_expressions() {
        let bound_v3 = BoundFormula {
            mode: FormulaMode::Dynamic,
            root: BoundExpr::Call {
                name: String::from("XLOOKUP"),
                args: vec![
                    BoundExpr::ScopedName {
                        sheet: Some(SheetId(1)),
                        missing_sheet: None,
                        name: String::from("Rate"),
                    },
                    BoundExpr::SpillRef(Box::new(BoundExpr::Reference(BoundRef::Cell {
                        sheet: None,
                        missing_sheet: None,
                        row: RowId(0),
                        col: ColId(0),
                        abs_col: false,
                        abs_row: false,
                    }))),
                    BoundExpr::Opaque {
                        raw: String::from("=_xlfn.FUTURE()"),
                    },
                    BoundExpr::MissingArg,
                    BoundExpr::PreservedCached {
                        raw: String::from("=LEGACY(1)"),
                        cached: ScalarValue::Number(42.5),
                    },
                ],
            },
        };
        assert!(formula_uses_v3(&bound_v3));

        let ops = vec![
            Op::SetCells {
                sheet: SheetId(0),
                edits: vec![CellEdit {
                    row: RowId(2),
                    col: ColId(3),
                    input: CellInput::Formula {
                        formula: bound_v3.clone(),
                        cached: Some(ScalarValue::Bool(true)),
                    },
                    format: FormatPatch::Keep,
                }],
            },
            Op::FillFormula {
                sheet: SheetId(0),
                rows: vec![reticula_core::id::IdRun::new(10, 3).unwrap()],
                cols: vec![reticula_core::id::IdRun::new(4, 1).unwrap()],
                template: TemplateFormula {
                    mode: FormulaMode::Dynamic,
                    root: TemplateExpr::Call {
                        name: String::from("SUM"),
                        args: vec![
                            TemplateExpr::SpillRef(Box::new(TemplateExpr::Reference(
                                TemplateRef::Cell {
                                    sheet: None,
                                    missing_sheet: None,
                                    row: AxisTarget::IdDelta(0),
                                    col: AxisTarget::Fixed(1),
                                    abs_col: true,
                                    abs_row: false,
                                },
                            ))),
                            TemplateExpr::MissingArg,
                        ],
                    },
                },
            },
            Op::PermuteContent {
                sheet: SheetId(0),
                rows: vec![RowId(0), RowId(1), RowId(2)],
                cols: vec![ColId(0), ColId(1)],
                row_perm: vec![2, 0, 1],
            },
            Op::MoveCells {
                src_sheet: SheetId(0),
                src_rows: vec![RowId(0), RowId(1)],
                src_cols: vec![ColId(0)],
                dst_sheet: SheetId(1),
                dst_rows: vec![RowId(5), RowId(6)],
                dst_cols: vec![ColId(2)],
            },
            Op::DefineNamedRange {
                scope: Some(SheetId(0)),
                name: String::from("TaxRate"),
                formula: BoundFormula {
                    mode: FormulaMode::Legacy,
                    root: BoundExpr::Literal(Literal::Number(0.08)),
                },
                comment: Some(String::from("Sales tax")),
                hidden: false,
            },
            Op::DeleteNamedRange {
                scope: None,
                name: String::from("OldName"),
            },
        ];

        let wire_txn = ops_to_wire_txn_v3(String::from("M3a ops"), &ops).unwrap();
        let encoded = postcard::to_allocvec(&wire_txn).unwrap();
        let decoded_txn = postcard::from_bytes(&encoded).unwrap();
        let (label, roundtrip) = ops_from_wire_txn_v3(decoded_txn).unwrap();
        assert_eq!(label, "M3a ops");
        assert_eq!(roundtrip, ops);

        for op in &wire_txn.ops {
            assert_eq!(
                WireOpV2::try_from(op),
                Err(WireConvertError::UnsupportedInV2)
            );
        }

        let frame_v2 = WireSyncFrameV2::Commit {
            seq: 1,
            replica: 1,
            client_seq: 1,
            user_id: [0x11; 16],
            timestamp_ms: 99,
            txn: WireOpV3::PermuteContent {
                sheet: 0,
                rows: vec![0, 1],
                cols: vec![0],
                row_perm: vec![1, 0],
            }
            .into_txn("Sort"),
        };
        assert_eq!(
            crate::v2::WireSyncFrameV1::try_from(&frame_v2),
            Err(WireConvertError::UnsupportedInV2)
        );
    }

    trait IntoTxnExt {
        fn into_txn(self, label: &str) -> crate::v3::WireTxnV3;
    }
    impl IntoTxnExt for WireOpV3 {
        fn into_txn(self, label: &str) -> crate::v3::WireTxnV3 {
            crate::v3::WireTxnV3 {
                label: String::from(label),
                ops: vec![self],
            }
        }
    }
}
