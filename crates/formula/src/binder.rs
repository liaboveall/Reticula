//! Coordinate-to-identity formula binder and A1 unparser (`13-m1a.md` 5.4).
//!
//! At authoring time, A1 coordinates are bound to stable [`RowId`] and [`ColId`]
//! anchors on the target worksheet's axes. Inserting or deleting rows and
//! columns never rewrites [`BoundFormula`]: unparsing resolves current live
//! positions from the bound anchors and emits `#REF!` when a single-cell target
//! has been tombstoned or when an entire anchor range contains zero live rows
//! or columns.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use reticula_core::digest::Digest64;
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{FormulaMode, FormulaPayload, OpError, Workbook};
use reticula_core::sheet::Sheet;
use serde::{Deserialize, Serialize};

use crate::address::{CellAddr, write_col_letters, write_sheet_prefix};
use crate::ast::{
    BinaryOp, Expr, Literal, MAX_FUNCTION_ARGS, MAX_NESTING_DEPTH, UnaryOp, write_literal,
};
use crate::parser::{ParseError, parse_formula};

/// Identity-bound cell or range reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundRef {
    /// Single cell anchored to `(row, col)`.
    Cell {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name if not found at bind time.
        missing_sheet: Option<String>,
        /// Target row identity.
        row: RowId,
        /// Target column identity.
        col: ColId,
        /// Original `$` flag on column.
        abs_col: bool,
        /// Original `$` flag on row.
        abs_row: bool,
    },
    /// Rectangular cell area anchored by inclusive corner identities.
    Area {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name if not found at bind time.
        missing_sheet: Option<String>,
        /// Top anchor row identity.
        first_row: RowId,
        /// Left anchor column identity.
        first_col: ColId,
        /// Bottom anchor row identity.
        last_row: RowId,
        /// Right anchor column identity.
        last_col: ColId,
        /// Original `$` flag on `first_col`.
        abs_first_col: bool,
        /// Original `$` flag on `first_row`.
        abs_first_row: bool,
        /// Original `$` flag on `last_col`.
        abs_last_col: bool,
        /// Original `$` flag on `last_row`.
        abs_last_row: bool,
    },
    /// Full-column range anchored by inclusive column identities.
    ColRange {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name if not found at bind time.
        missing_sheet: Option<String>,
        /// Left anchor column identity.
        first_col: ColId,
        /// Right anchor column identity.
        last_col: ColId,
        /// Original `$` flag on `first_col`.
        abs_first: bool,
        /// Original `$` flag on `last_col`.
        abs_last: bool,
    },
    /// Full-row range anchored by inclusive row identities.
    RowRange {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name if not found at bind time.
        missing_sheet: Option<String>,
        /// Top anchor row identity.
        first_row: RowId,
        /// Bottom anchor row identity.
        last_row: RowId,
        /// Original `$` flag on `first_row`.
        abs_first: bool,
        /// Original `$` flag on `last_row`.
        abs_last: bool,
    },
    /// Out-of-bounds or deleted reference (`#REF!`).
    Invalid {
        /// Optional sheet name prefix retained for display if present.
        sheet_name: Option<String>,
    },
}

/// Identity-bound expression tree stored inside [`Workbook`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BoundExpr {
    /// Scalar literal leaf.
    Literal(Literal),
    /// Constant `{...}` array literal.
    ArrayLiteral {
        /// Row count (`>= 1`).
        rows: u32,
        /// Column count (`>= 1`).
        cols: u32,
        /// Row-major literal items.
        elements: Vec<Literal>,
    },
    /// Identity-bound cell or range reference.
    Reference(BoundRef),
    /// Unresolved name (evaluates to `#NAME?`).
    Name(String),
    /// Explicit parentheses `(inner)` — suppresses outermost tail-zeroing on
    /// `=(A1-B1)`.
    Paren(Box<Self>),
    /// Prefix or postfix unary operator.
    Unary {
        /// Operator kind.
        op: UnaryOp,
        /// Operand expression.
        expr: Box<Self>,
    },
    /// Binary infix operator.
    Binary {
        /// Operator kind.
        op: BinaryOp,
        /// Left operand.
        left: Box<Self>,
        /// Right operand.
        right: Box<Self>,
    },
    /// Function call (`SUM(...)`, `ROW()`, etc.).
    Call {
        /// Uppercase function name.
        name: String,
        /// Bound argument expressions.
        args: Vec<Self>,
    },
}

/// Complete bound formula stored in a workbook cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoundFormula {
    /// Dynamic vs legacy evaluation mode (D-028).
    pub mode: FormulaMode,
    /// Root bound expression.
    pub root: BoundExpr,
}

impl FormulaPayload for BoundFormula {
    fn estimated_bytes(&self) -> usize {
        size_of::<Self>().saturating_add(expr_bytes(&self.root))
    }

    fn validate(&self) -> Result<(), OpError> {
        validate_bound_expr(&self.root, 0)
    }

    fn write_digest(&self, digest: &mut Digest64) {
        let mode_byte = match self.mode {
            FormulaMode::Dynamic => 1_u8,
            FormulaMode::Legacy => 2_u8,
        };
        digest.write_bytes(&[mode_byte]);
        write_expr_digest(&self.root, digest);
    }
}

impl BoundFormula {
    /// Parses and binds `source` against `workbook` on `host_sheet`.
    pub fn parse_and_bind(
        source: &str,
        mode: FormulaMode,
        workbook: &Workbook<Self>,
        host_sheet: SheetId,
    ) -> Result<Self, ParseError> {
        let ast = parse_formula(source)?;
        Ok(Self {
            mode,
            root: bind_expr(&ast, workbook, host_sheet),
        })
    }

    /// Binds a pre-parsed [`Expr`] against a single [`Sheet`] (used when every
    /// reference targets `host_sheet`).
    #[must_use]
    pub fn bind_on_sheet(ast: &Expr, mode: FormulaMode, sheet: &Sheet) -> Self {
        Self {
            mode,
            root: bind_expr_single_sheet(ast, sheet),
        }
    }

    /// Whether this formula invokes structure-sensitive functions (`ROW` or
    /// `COLUMN`) that must recalculate when rows or columns are inserted or
    /// deleted.
    #[must_use]
    pub fn is_structure_sensitive(&self) -> bool {
        expr_is_structure_sensitive(&self.root)
    }

    /// Reconstructs the formula string (beginning with `'='`) from current live
    /// axis positions in `workbook` for a cell on `host_sheet`.
    #[must_use]
    pub fn unparse(&self, workbook: &Workbook<Self>, host_sheet: SheetId) -> String {
        let mut out = String::from("=");
        unparse_expr(&self.root, workbook, host_sheet, &mut out, None);
        out
    }

    /// Reconstructs the formula string against a single [`Sheet`].
    #[must_use]
    pub fn unparse_on_sheet(&self, sheet: &Sheet) -> String {
        let mut out = String::from("=");
        unparse_expr_single_sheet(&self.root, sheet, &mut out, None);
        out
    }
}

/// Binds a coordinate [`Expr`] into a [`BoundExpr`] using `workbook` axes.
#[must_use]
pub fn bind_expr(ast: &Expr, workbook: &Workbook<BoundFormula>, host_sheet: SheetId) -> BoundExpr {
    match ast {
        Expr::Literal(lit) => BoundExpr::Literal(lit.clone()),
        Expr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => BoundExpr::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.clone(),
        },
        Expr::CellRef { sheet, addr } => {
            BoundExpr::Reference(bind_cell_ref(sheet.as_deref(), *addr, workbook, host_sheet))
        }
        Expr::AreaRef { sheet, first, last } => BoundExpr::Reference(bind_area_ref(
            sheet.as_deref(),
            *first,
            *last,
            workbook,
            host_sheet,
        )),
        Expr::ColRange {
            sheet,
            first_col,
            abs_first,
            last_col,
            abs_last,
        } => BoundExpr::Reference(bind_col_range(
            sheet.as_deref(),
            *first_col,
            *abs_first,
            *last_col,
            *abs_last,
            workbook,
            host_sheet,
        )),
        Expr::RowRange {
            sheet,
            first_row,
            abs_first,
            last_row,
            abs_last,
        } => BoundExpr::Reference(bind_row_range(
            sheet.as_deref(),
            *first_row,
            *abs_first,
            *last_row,
            *abs_last,
            workbook,
            host_sheet,
        )),
        Expr::Name(name) => BoundExpr::Name(name.clone()),
        Expr::Paren(inner) => BoundExpr::Paren(Box::new(bind_expr(inner, workbook, host_sheet))),
        Expr::Unary { op, expr } => BoundExpr::Unary {
            op: *op,
            expr: Box::new(bind_expr(expr, workbook, host_sheet)),
        },
        Expr::Binary { op, left, right } => BoundExpr::Binary {
            op: *op,
            left: Box::new(bind_expr(left, workbook, host_sheet)),
            right: Box::new(bind_expr(right, workbook, host_sheet)),
        },
        Expr::Call { name, args } => BoundExpr::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|arg| bind_expr(arg, workbook, host_sheet))
                .collect(),
        },
    }
}

fn bind_expr_single_sheet(ast: &Expr, sheet: &Sheet) -> BoundExpr {
    match ast {
        Expr::Literal(lit) => BoundExpr::Literal(lit.clone()),
        Expr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => BoundExpr::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.clone(),
        },
        Expr::CellRef { sheet: None, addr } => {
            BoundExpr::Reference(bind_cell_on_sheet(None, None, *addr, sheet))
        }
        Expr::AreaRef {
            sheet: None,
            first,
            last,
        } => BoundExpr::Reference(bind_area_on_sheet(None, None, *first, *last, sheet)),
        Expr::ColRange {
            sheet: None,
            first_col,
            abs_first,
            last_col,
            abs_last,
        } => BoundExpr::Reference(bind_cols_on_sheet(
            None, None, *first_col, *abs_first, *last_col, *abs_last, sheet,
        )),
        Expr::RowRange {
            sheet: None,
            first_row,
            abs_first,
            last_row,
            abs_last,
        } => BoundExpr::Reference(bind_rows_on_sheet(
            None, None, *first_row, *abs_first, *last_row, *abs_last, sheet,
        )),
        Expr::CellRef { sheet: Some(s), .. }
        | Expr::AreaRef { sheet: Some(s), .. }
        | Expr::ColRange { sheet: Some(s), .. }
        | Expr::RowRange { sheet: Some(s), .. } => BoundExpr::Reference(BoundRef::Invalid {
            sheet_name: Some(s.clone()),
        }),
        Expr::Name(name) => BoundExpr::Name(name.clone()),
        Expr::Paren(inner) => BoundExpr::Paren(Box::new(bind_expr_single_sheet(inner, sheet))),
        Expr::Unary { op, expr } => BoundExpr::Unary {
            op: *op,
            expr: Box::new(bind_expr_single_sheet(expr, sheet)),
        },
        Expr::Binary { op, left, right } => BoundExpr::Binary {
            op: *op,
            left: Box::new(bind_expr_single_sheet(left, sheet)),
            right: Box::new(bind_expr_single_sheet(right, sheet)),
        },
        Expr::Call { name, args } => BoundExpr::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|arg| bind_expr_single_sheet(arg, sheet))
                .collect(),
        },
    }
}

fn resolve_target_sheet<'a>(
    sheet_name: Option<&str>,
    workbook: &'a Workbook<BoundFormula>,
    host_sheet: SheetId,
) -> Option<(Option<SheetId>, &'a Sheet)> {
    match sheet_name {
        None => workbook.sheet(host_sheet).map(|s| (None, s)),
        Some(name) => workbook
            .sheets()
            .iter()
            .find(|s| s.name().eq_ignore_ascii_case(name))
            .map(|s| (Some(s.id()), s)),
    }
}

fn bind_cell_ref(
    sheet_name: Option<&str>,
    addr: CellAddr,
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
) -> BoundRef {
    let Some((qualified_id, target_sheet)) = resolve_target_sheet(sheet_name, workbook, host_sheet)
    else {
        return BoundRef::Invalid {
            sheet_name: sheet_name.map(String::from),
        };
    };
    bind_cell_on_sheet(qualified_id, None, addr, target_sheet)
}

fn bind_cell_on_sheet(
    sheet: Option<SheetId>,
    missing_sheet: Option<String>,
    addr: CellAddr,
    target: &Sheet,
) -> BoundRef {
    let (Some(row_id), Some(col_id)) = (
        target.row_axis().pos_to_id(addr.row),
        target.col_axis().pos_to_id(addr.col),
    ) else {
        return BoundRef::Invalid {
            sheet_name: missing_sheet,
        };
    };
    BoundRef::Cell {
        sheet,
        missing_sheet,
        row: RowId(row_id),
        col: ColId(col_id),
        abs_col: addr.abs_col,
        abs_row: addr.abs_row,
    }
}

fn bind_area_ref(
    sheet_name: Option<&str>,
    first: CellAddr,
    last: CellAddr,
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
) -> BoundRef {
    let Some((qualified_id, target_sheet)) = resolve_target_sheet(sheet_name, workbook, host_sheet)
    else {
        return BoundRef::Invalid {
            sheet_name: sheet_name.map(String::from),
        };
    };
    bind_area_on_sheet(qualified_id, None, first, last, target_sheet)
}

fn bind_area_on_sheet(
    sheet: Option<SheetId>,
    missing_sheet: Option<String>,
    first: CellAddr,
    last: CellAddr,
    target: &Sheet,
) -> BoundRef {
    let (top_row, abs_first_row, bot_row, abs_last_row) = if first.row <= last.row {
        (first.row, first.abs_row, last.row, last.abs_row)
    } else {
        (last.row, last.abs_row, first.row, first.abs_row)
    };
    let (left_col, abs_first_col, right_col, abs_last_col) = if first.col <= last.col {
        (first.col, first.abs_col, last.col, last.abs_col)
    } else {
        (last.col, last.abs_col, first.col, first.abs_col)
    };
    let (Some(r1), Some(c1), Some(r2), Some(c2)) = (
        target.row_axis().pos_to_id(top_row),
        target.col_axis().pos_to_id(left_col),
        target.row_axis().pos_to_id(bot_row),
        target.col_axis().pos_to_id(right_col),
    ) else {
        return BoundRef::Invalid {
            sheet_name: missing_sheet,
        };
    };
    BoundRef::Area {
        sheet,
        missing_sheet,
        first_row: RowId(r1),
        first_col: ColId(c1),
        last_row: RowId(r2),
        last_col: ColId(c2),
        abs_first_col,
        abs_first_row,
        abs_last_col,
        abs_last_row,
    }
}

fn bind_col_range(
    sheet_name: Option<&str>,
    first_col: u32,
    abs_first: bool,
    last_col: u32,
    abs_last: bool,
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
) -> BoundRef {
    let Some((qualified_id, target_sheet)) = resolve_target_sheet(sheet_name, workbook, host_sheet)
    else {
        return BoundRef::Invalid {
            sheet_name: sheet_name.map(String::from),
        };
    };
    bind_cols_on_sheet(
        qualified_id,
        None,
        first_col,
        abs_first,
        last_col,
        abs_last,
        target_sheet,
    )
}

fn bind_cols_on_sheet(
    sheet: Option<SheetId>,
    missing_sheet: Option<String>,
    first_col: u32,
    abs_first: bool,
    last_col: u32,
    abs_last: bool,
    target: &Sheet,
) -> BoundRef {
    let (c_min, af, c_max, al) = if first_col <= last_col {
        (first_col, abs_first, last_col, abs_last)
    } else {
        (last_col, abs_last, first_col, abs_first)
    };
    let (Some(c1), Some(c2)) = (
        target.col_axis().pos_to_id(c_min),
        target.col_axis().pos_to_id(c_max),
    ) else {
        return BoundRef::Invalid {
            sheet_name: missing_sheet,
        };
    };
    BoundRef::ColRange {
        sheet,
        missing_sheet,
        first_col: ColId(c1),
        last_col: ColId(c2),
        abs_first: af,
        abs_last: al,
    }
}

fn bind_row_range(
    sheet_name: Option<&str>,
    first_row: u32,
    abs_first: bool,
    last_row: u32,
    abs_last: bool,
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
) -> BoundRef {
    let Some((qualified_id, target_sheet)) = resolve_target_sheet(sheet_name, workbook, host_sheet)
    else {
        return BoundRef::Invalid {
            sheet_name: sheet_name.map(String::from),
        };
    };
    bind_rows_on_sheet(
        qualified_id,
        None,
        first_row,
        abs_first,
        last_row,
        abs_last,
        target_sheet,
    )
}

fn bind_rows_on_sheet(
    sheet: Option<SheetId>,
    missing_sheet: Option<String>,
    first_row: u32,
    abs_first: bool,
    last_row: u32,
    abs_last: bool,
    target: &Sheet,
) -> BoundRef {
    let (r_min, af, r_max, al) = if first_row <= last_row {
        (first_row, abs_first, last_row, abs_last)
    } else {
        (last_row, abs_last, first_row, abs_first)
    };
    let (Some(r1), Some(r2)) = (
        target.row_axis().pos_to_id(r_min),
        target.row_axis().pos_to_id(r_max),
    ) else {
        return BoundRef::Invalid {
            sheet_name: missing_sheet,
        };
    };
    BoundRef::RowRange {
        sheet,
        missing_sheet,
        first_row: RowId(r1),
        last_row: RowId(r2),
        abs_first: af,
        abs_last: al,
    }
}

fn unparse_expr(
    expr: &BoundExpr,
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
    out: &mut String,
    parent_prec: Option<(u8, bool)>,
) {
    match expr {
        BoundExpr::Literal(lit) => write_literal(lit, out),
        BoundExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => write_array_literal(*rows, *cols, elements, out),
        BoundExpr::Reference(r) => unparse_ref(r, workbook, host_sheet, out),
        BoundExpr::Name(name) => out.push_str(name),
        BoundExpr::Paren(inner) => {
            out.push('(');
            unparse_expr(inner, workbook, host_sheet, out, None);
            out.push(')');
        }
        BoundExpr::Unary { op, expr: inner } => {
            let my_prec = unary_prec(*op);
            let wrap = parent_prec.is_some_and(|(p, _)| my_prec < p);
            if wrap {
                out.push('(');
            }
            match op {
                UnaryOp::Plus => {
                    out.push('+');
                    unparse_expr(inner, workbook, host_sheet, out, Some((my_prec, false)));
                }
                UnaryOp::Neg => {
                    out.push('-');
                    unparse_expr(inner, workbook, host_sheet, out, Some((my_prec, false)));
                }
                UnaryOp::ImplicitIntersection => {
                    out.push('@');
                    unparse_expr(inner, workbook, host_sheet, out, Some((my_prec, false)));
                }
                UnaryOp::Percent => {
                    unparse_expr(inner, workbook, host_sheet, out, Some((my_prec, false)));
                    out.push('%');
                }
            }
            if wrap {
                out.push(')');
            }
        }
        BoundExpr::Binary { op, left, right } => {
            let my_prec = binary_prec(*op);
            let wrap =
                parent_prec.is_some_and(|(p, is_right)| my_prec < p || (is_right && my_prec == p));
            if wrap {
                out.push('(');
            }
            unparse_expr(left, workbook, host_sheet, out, Some((my_prec, false)));
            out.push_str(op.as_str());
            unparse_expr(right, workbook, host_sheet, out, Some((my_prec, true)));
            if wrap {
                out.push(')');
            }
        }
        BoundExpr::Call { name, args } => {
            out.push_str(name);
            out.push('(');
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                unparse_expr(arg, workbook, host_sheet, out, None);
            }
            out.push(')');
        }
    }
}

fn unparse_expr_single_sheet(
    expr: &BoundExpr,
    sheet: &Sheet,
    out: &mut String,
    parent_prec: Option<(u8, bool)>,
) {
    match expr {
        BoundExpr::Literal(lit) => write_literal(lit, out),
        BoundExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => write_array_literal(*rows, *cols, elements, out),
        BoundExpr::Reference(r) => unparse_ref_on_sheet(r, None, Some(sheet), out),
        BoundExpr::Name(name) => out.push_str(name),
        BoundExpr::Paren(inner) => {
            out.push('(');
            unparse_expr_single_sheet(inner, sheet, out, None);
            out.push(')');
        }
        BoundExpr::Unary { op, expr: inner } => {
            let my_prec = unary_prec(*op);
            let wrap = parent_prec.is_some_and(|(p, _)| my_prec < p);
            if wrap {
                out.push('(');
            }
            match op {
                UnaryOp::Plus => {
                    out.push('+');
                    unparse_expr_single_sheet(inner, sheet, out, Some((my_prec, false)));
                }
                UnaryOp::Neg => {
                    out.push('-');
                    unparse_expr_single_sheet(inner, sheet, out, Some((my_prec, false)));
                }
                UnaryOp::ImplicitIntersection => {
                    out.push('@');
                    unparse_expr_single_sheet(inner, sheet, out, Some((my_prec, false)));
                }
                UnaryOp::Percent => {
                    unparse_expr_single_sheet(inner, sheet, out, Some((my_prec, false)));
                    out.push('%');
                }
            }
            if wrap {
                out.push(')');
            }
        }
        BoundExpr::Binary { op, left, right } => {
            let my_prec = binary_prec(*op);
            let wrap =
                parent_prec.is_some_and(|(p, is_right)| my_prec < p || (is_right && my_prec == p));
            if wrap {
                out.push('(');
            }
            unparse_expr_single_sheet(left, sheet, out, Some((my_prec, false)));
            out.push_str(op.as_str());
            unparse_expr_single_sheet(right, sheet, out, Some((my_prec, true)));
            if wrap {
                out.push(')');
            }
        }
        BoundExpr::Call { name, args } => {
            out.push_str(name);
            out.push('(');
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                unparse_expr_single_sheet(arg, sheet, out, None);
            }
            out.push(')');
        }
    }
}

fn unparse_ref(
    bound: &BoundRef,
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
    out: &mut String,
) {
    let target_sheet_id = match bound {
        BoundRef::Cell { sheet, .. }
        | BoundRef::Area { sheet, .. }
        | BoundRef::ColRange { sheet, .. }
        | BoundRef::RowRange { sheet, .. } => sheet.unwrap_or(host_sheet),
        BoundRef::Invalid { .. } => {
            out.push_str("#REF!");
            return;
        }
    };
    let target = workbook.sheet(target_sheet_id);
    let prefix = match bound {
        BoundRef::Cell {
            sheet: Some(id), ..
        }
        | BoundRef::Area {
            sheet: Some(id), ..
        }
        | BoundRef::ColRange {
            sheet: Some(id), ..
        }
        | BoundRef::RowRange {
            sheet: Some(id), ..
        } => workbook.sheet(*id).map(Sheet::name),
        _ => None,
    };
    unparse_ref_on_sheet(bound, prefix, target, out);
}

fn unparse_ref_on_sheet(
    bound: &BoundRef,
    prefix: Option<&str>,
    target: Option<&Sheet>,
    out: &mut String,
) {
    let Some(sheet) = target else {
        out.push_str("#REF!");
        return;
    };
    match bound {
        BoundRef::Invalid { .. } => out.push_str("#REF!"),
        BoundRef::Cell {
            row,
            col,
            abs_col,
            abs_row,
            ..
        } => {
            let (Some(r_pos), Some(c_pos)) = (
                sheet.row_axis().id_to_pos(row.0),
                sheet.col_axis().id_to_pos(col.0),
            ) else {
                out.push_str("#REF!");
                return;
            };
            write_sheet_prefix(prefix, out);
            CellAddr {
                col: c_pos,
                row: r_pos,
                abs_col: *abs_col,
                abs_row: *abs_row,
            }
            .write_a1(out);
        }
        BoundRef::Area {
            first_row,
            first_col,
            last_row,
            last_col,
            abs_first_col,
            abs_first_row,
            abs_last_col,
            abs_last_row,
            ..
        } => {
            let Some((r1_pos, r2_pos)) =
                live_span_positions(sheet.row_axis(), first_row.0, last_row.0)
            else {
                out.push_str("#REF!");
                return;
            };
            let Some((c1_pos, c2_pos)) =
                live_span_positions(sheet.col_axis(), first_col.0, last_col.0)
            else {
                out.push_str("#REF!");
                return;
            };
            write_sheet_prefix(prefix, out);
            CellAddr {
                col: c1_pos,
                row: r1_pos,
                abs_col: *abs_first_col,
                abs_row: *abs_first_row,
            }
            .write_a1(out);
            out.push(':');
            CellAddr {
                col: c2_pos,
                row: r2_pos,
                abs_col: *abs_last_col,
                abs_row: *abs_last_row,
            }
            .write_a1(out);
        }
        BoundRef::ColRange {
            first_col,
            last_col,
            abs_first,
            abs_last,
            ..
        } => {
            let Some((c1_pos, c2_pos)) =
                live_span_positions(sheet.col_axis(), first_col.0, last_col.0)
            else {
                out.push_str("#REF!");
                return;
            };
            write_sheet_prefix(prefix, out);
            if *abs_first {
                out.push('$');
            }
            write_col_letters(c1_pos, out);
            out.push(':');
            if *abs_last {
                out.push('$');
            }
            write_col_letters(c2_pos, out);
        }
        BoundRef::RowRange {
            first_row,
            last_row,
            abs_first,
            abs_last,
            ..
        } => {
            let Some((r1_pos, r2_pos)) =
                live_span_positions(sheet.row_axis(), first_row.0, last_row.0)
            else {
                out.push_str("#REF!");
                return;
            };
            write_sheet_prefix(prefix, out);
            if *abs_first {
                out.push('$');
            }
            let one_r1 = u64::from(r1_pos).saturating_add(1);
            let _ = write!(out, "{one_r1}:");
            if *abs_last {
                out.push('$');
            }
            let one_r2 = u64::from(r2_pos).saturating_add(1);
            let _ = write!(out, "{one_r2}");
        }
    }
}

/// Returns `(first_live_pos, last_live_pos)` between `first_anchor` and
/// `last_anchor` on `axis`, or `None` if all entries in the anchor span are
/// tombstoned.
#[must_use]
pub fn live_span_positions(
    axis: &reticula_core::axis::Axis,
    first_anchor: u64,
    last_anchor: u64,
) -> Option<(u32, u32)> {
    let mut first_id: Option<u64> = None;
    let mut last_id: Option<u64> = None;
    let ok = axis.for_each_live_in_range(first_anchor, last_anchor, |run| {
        if first_id.is_none() {
            first_id = Some(run.first_id);
        }
        if let Some(end_id) = run.id_at(run.len.saturating_sub(1)) {
            last_id = Some(end_id);
        }
    });
    if !ok {
        return None;
    }
    let (f_id, l_id) = (first_id?, last_id?);
    Some((axis.id_to_pos(f_id)?, axis.id_to_pos(l_id)?))
}

fn write_array_literal(rows: u32, cols: u32, elements: &[Literal], out: &mut String) {
    out.push('{');
    for r in 0..rows {
        if r > 0 {
            out.push(';');
        }
        for c in 0..cols {
            if c > 0 {
                out.push(',');
            }
            let idx = (r as usize)
                .saturating_mul(cols as usize)
                .saturating_add(c as usize);
            if let Some(lit) = elements.get(idx) {
                write_literal(lit, out);
            }
        }
    }
    out.push('}');
}

const fn unary_prec(op: UnaryOp) -> u8 {
    match op {
        UnaryOp::Percent => 80,
        UnaryOp::Plus | UnaryOp::Neg | UnaryOp::ImplicitIntersection => 70,
    }
}

const fn binary_prec(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Pow => 60,
        BinaryOp::Mul | BinaryOp::Div => 50,
        BinaryOp::Add | BinaryOp::Sub => 40,
        BinaryOp::Concat => 30,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
            20
        }
    }
}

fn expr_is_structure_sensitive(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Reference(_)
        | BoundExpr::Name(_) => false,
        BoundExpr::Paren(inner) | BoundExpr::Unary { expr: inner, .. } => {
            expr_is_structure_sensitive(inner)
        }
        BoundExpr::Binary { left, right, .. } => {
            expr_is_structure_sensitive(left) || expr_is_structure_sensitive(right)
        }
        BoundExpr::Call { name, args } => {
            name == "ROW" || name == "COLUMN" || args.iter().any(expr_is_structure_sensitive)
        }
    }
}

fn expr_bytes(expr: &BoundExpr) -> usize {
    let base = size_of::<BoundExpr>();
    match expr {
        BoundExpr::Literal(Literal::Text(s)) | BoundExpr::Name(s) => {
            base.saturating_add(s.capacity())
        }
        BoundExpr::Literal(_) | BoundExpr::Reference(_) => base,
        BoundExpr::ArrayLiteral { elements, .. } => {
            base.saturating_add(elements.capacity().saturating_mul(size_of::<Literal>()))
        }
        BoundExpr::Paren(inner) | BoundExpr::Unary { expr: inner, .. } => {
            base.saturating_add(expr_bytes(inner))
        }
        BoundExpr::Binary { left, right, .. } => base
            .saturating_add(expr_bytes(left))
            .saturating_add(expr_bytes(right)),
        BoundExpr::Call { name, args } => args
            .iter()
            .fold(base.saturating_add(name.capacity()), |acc, a| {
                acc.saturating_add(expr_bytes(a))
            }),
    }
}

fn validate_bound_expr(expr: &BoundExpr, depth: usize) -> Result<(), OpError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(OpError::InvalidFormula);
    }
    match expr {
        BoundExpr::Literal(Literal::Number(n)) => {
            if !n.is_finite() {
                return Err(OpError::NonFiniteNumber);
            }
            Ok(())
        }
        BoundExpr::Literal(_) | BoundExpr::Reference(_) | BoundExpr::Name(_) => Ok(()),
        BoundExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => {
            let expected = (*rows as usize).checked_mul(*cols as usize);
            if *rows == 0 || *cols == 0 || expected != Some(elements.len()) {
                return Err(OpError::InvalidFormula);
            }
            for item in elements {
                if let Literal::Number(n) = item
                    && !n.is_finite()
                {
                    return Err(OpError::NonFiniteNumber);
                }
            }
            Ok(())
        }
        BoundExpr::Paren(inner) | BoundExpr::Unary { expr: inner, .. } => {
            validate_bound_expr(inner, depth.saturating_add(1))
        }
        BoundExpr::Binary { left, right, .. } => {
            validate_bound_expr(left, depth.saturating_add(1))?;
            validate_bound_expr(right, depth.saturating_add(1))
        }
        BoundExpr::Call { args, .. } => {
            if args.len() > MAX_FUNCTION_ARGS {
                return Err(OpError::InvalidFormula);
            }
            let next_depth = depth.saturating_add(1);
            for arg in args {
                validate_bound_expr(arg, next_depth)?;
            }
            Ok(())
        }
    }
}

fn write_expr_digest(expr: &BoundExpr, d: &mut Digest64) {
    match expr {
        BoundExpr::Literal(lit) => {
            d.write_bytes(&[1]);
            write_lit_digest(lit, d);
        }
        BoundExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => {
            d.write_bytes(&[2]);
            d.write_u32(*rows);
            d.write_u32(*cols);
            for lit in elements {
                write_lit_digest(lit, d);
            }
        }
        BoundExpr::Reference(r) => {
            d.write_bytes(&[3]);
            write_ref_digest(r, d);
        }
        BoundExpr::Name(s) => {
            d.write_bytes(&[4]);
            d.write_bytes(s.as_bytes());
        }
        BoundExpr::Paren(inner) => {
            d.write_bytes(&[5]);
            write_expr_digest(inner, d);
        }
        BoundExpr::Unary { op, expr: inner } => {
            let tag = match op {
                UnaryOp::Plus => 1_u8,
                UnaryOp::Neg => 2_u8,
                UnaryOp::ImplicitIntersection => 3_u8,
                UnaryOp::Percent => 4_u8,
            };
            d.write_bytes(&[6, tag]);
            write_expr_digest(inner, d);
        }
        BoundExpr::Binary { op, left, right } => {
            d.write_bytes(&[7]);
            d.write_bytes(op.as_str().as_bytes());
            write_expr_digest(left, d);
            write_expr_digest(right, d);
        }
        BoundExpr::Call { name, args } => {
            d.write_bytes(&[8]);
            d.write_bytes(name.as_bytes());
            d.write_u32(u32::try_from(args.len()).unwrap_or(u32::MAX));
            for arg in args {
                write_expr_digest(arg, d);
            }
        }
    }
}

fn write_lit_digest(lit: &Literal, d: &mut Digest64) {
    match lit {
        Literal::Number(n) => {
            d.write_bytes(&[1]);
            d.write_f64(*n);
        }
        Literal::Text(s) => {
            d.write_bytes(&[2]);
            d.write_bytes(s.as_bytes());
        }
        Literal::Bool(b) => d.write_bytes(&[3, u8::from(*b)]),
        Literal::Error(e) => d.write_bytes(&[4, *e as u8]),
    }
}

fn write_ref_digest(r: &BoundRef, d: &mut Digest64) {
    match r {
        BoundRef::Cell {
            sheet,
            row,
            col,
            abs_col,
            abs_row,
            ..
        } => {
            d.write_bytes(&[1, u8::from(*abs_col), u8::from(*abs_row)]);
            d.write_u32(sheet.map_or(u32::MAX, |s| s.0));
            d.write_u64(row.0);
            d.write_u64(col.0);
        }
        BoundRef::Area {
            sheet,
            first_row,
            first_col,
            last_row,
            last_col,
            abs_first_col,
            abs_first_row,
            abs_last_col,
            abs_last_row,
            ..
        } => {
            d.write_bytes(&[
                2,
                u8::from(*abs_first_col),
                u8::from(*abs_first_row),
                u8::from(*abs_last_col),
                u8::from(*abs_last_row),
            ]);
            d.write_u32(sheet.map_or(u32::MAX, |s| s.0));
            d.write_u64(first_row.0);
            d.write_u64(first_col.0);
            d.write_u64(last_row.0);
            d.write_u64(last_col.0);
        }
        BoundRef::ColRange {
            sheet,
            first_col,
            last_col,
            abs_first,
            abs_last,
            ..
        } => {
            d.write_bytes(&[3, u8::from(*abs_first), u8::from(*abs_last)]);
            d.write_u32(sheet.map_or(u32::MAX, |s| s.0));
            d.write_u64(first_col.0);
            d.write_u64(last_col.0);
        }
        BoundRef::RowRange {
            sheet,
            first_row,
            last_row,
            abs_first,
            abs_last,
            ..
        } => {
            d.write_bytes(&[4, u8::from(*abs_first), u8::from(*abs_last)]);
            d.write_u32(sheet.map_or(u32::MAX, |s| s.0));
            d.write_u64(first_row.0);
            d.write_u64(last_row.0);
        }
        BoundRef::Invalid { sheet_name } => {
            d.write_bytes(&[5]);
            if let Some(s) = sheet_name {
                d.write_bytes(s.as_bytes());
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::vec;

    use reticula_core::id::{IdRun, RowId, SheetId};
    use reticula_core::ops::{FormulaMode, Op, Workbook};

    use super::BoundFormula;

    #[test]
    fn bound_formulas_follow_row_insertions_and_turn_deleted_targets_into_ref_error() {
        let mut wb: Workbook<BoundFormula> = Workbook::new();
        let sheet = SheetId(0);

        let f_cell =
            BoundFormula::parse_and_bind("=A2+$V$1", FormulaMode::Dynamic, &wb, sheet).unwrap();
        let f_area =
            BoundFormula::parse_and_bind("=SUM(M2:M5)", FormulaMode::Dynamic, &wb, sheet).unwrap();
        assert!(!f_cell.is_structure_sensitive());

        // Insert 2 rows at top: `A2` becomes `A4`, `$V$1` becomes `$V$3`, `M2:M5` becomes `M4:M7`.
        let inserted = wb.allocator_mut().alloc_run(2).unwrap();
        wb.apply(Op::InsertRows {
            sheet,
            after: None,
            ids: inserted,
        })
        .unwrap();
        assert_eq!(f_cell.unparse(&wb, sheet), "=A4+$V$3");
        assert_eq!(f_area.unparse(&wb, sheet), "=SUM(M4:M7)");

        // Delete original row 1 (base ID 1, which was `A2` / `M2`):
        // single-cell `A2` becomes `#REF!`, while `M2:M5` shrinks to its 3 surviving rows `M4:M6`!
        wb.apply(Op::DeleteRows {
            sheet,
            ids: vec![IdRun::new(1, 1).unwrap()],
        })
        .unwrap();
        assert_eq!(f_cell.unparse(&wb, sheet), "=#REF!+$V$3");
        assert_eq!(f_area.unparse(&wb, sheet), "=SUM(M4:M6)");

        // Deleting the remaining rows of `M2:M5` (base IDs 2, 3, 4) turns the area into `#REF!`.
        wb.apply(Op::DeleteRows {
            sheet,
            ids: vec![IdRun::new(2, 3).unwrap()],
        })
        .unwrap();
        assert_eq!(f_area.unparse(&wb, sheet), "=SUM(#REF!)");

        // Structural sensitivity detection.
        let f_row =
            BoundFormula::parse_and_bind("=ROW()+1", FormulaMode::Dynamic, &wb, sheet).unwrap();
        assert!(f_row.is_structure_sensitive());
        let _ = RowId(0);
    }
}
