//! Abstract syntax tree of parsed formulas and canonical A1 pretty-printing.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use reticula_core::format::{NumberFormat, write_number};
use reticula_core::value::CalcError;
use serde::{Deserialize, Serialize};

use crate::address::{CellAddr, write_col_letters, write_sheet_prefix};

/// Maximum parenthesis / function / array nesting depth (`13-m1a.md` 5.3).
pub const MAX_NESTING_DEPTH: usize = 64;

/// Maximum arguments to a single function call (`13-m1a.md` 5.3).
pub const MAX_FUNCTION_ARGS: usize = 255;

/// Prefix or postfix unary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnaryOp {
    /// Unary `+x`.
    Plus,
    /// Unary `-x`.
    Neg,
    /// Explicit implicit-intersection `@x` (D-028).
    ImplicitIntersection,
    /// Postfix percentage `x%` (`x / 100`).
    Percent,
}

/// Binary infix operators (excluding `:`, which forms reference nodes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BinaryOp {
    /// Exponentiation `a ^ b` (left-associative in Excel).
    Pow,
    /// Multiplication `a * b`.
    Mul,
    /// Division `a / b`.
    Div,
    /// Addition `a + b`.
    Add,
    /// Subtraction `a - b`.
    Sub,
    /// Text concatenation `a & b`.
    Concat,
    /// Equal `a = b`.
    Eq,
    /// Not equal `a <> b`.
    Ne,
    /// Less than `a < b`.
    Lt,
    /// Greater than `a > b`.
    Gt,
    /// Less than or equal `a <= b`.
    Le,
    /// Greater than or equal `a >= b`.
    Ge,
}

impl BinaryOp {
    /// Canonical operator token string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pow => "^",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Add => "+",
            Self::Sub => "-",
            Self::Concat => "&",
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Gt => ">",
            Self::Le => "<=",
            Self::Ge => ">=",
        }
    }
}

/// Scalar constant allowed inside array literals `{1,2;3,4}` and AST leaves.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Literal {
    /// Finite IEEE 754 number (`-0.0` canonicalised to `+0.0`).
    Number(f64),
    /// Double-quoted text literal.
    Text(String),
    /// Boolean literal (`TRUE` or `FALSE`).
    Bool(bool),
    /// Error literal (`#REF!`, `#DIV/0!`, etc.).
    Error(CalcError),
}

/// Coordinate-addressed syntax tree produced by the Pratt parser.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    /// Scalar literal leaf.
    Literal(Literal),
    /// Rectangular array constant `{1,2;3,4}`.
    ArrayLiteral {
        /// Number of rows (`>= 1`).
        rows: u32,
        /// Number of columns (`>= 1`).
        cols: u32,
        /// Row-major literal elements (`len == rows * cols`).
        elements: Vec<Literal>,
    },
    /// Single cell reference (`A1`, `$A$1`, `Sheet1!A1`).
    CellRef {
        /// Optional sheet qualifier.
        sheet: Option<String>,
        /// Coordinate and `$` flags.
        addr: CellAddr,
    },
    /// Rectangular cell range (`A1:B10`, `Sheet1!$A$1:$B$10`).
    AreaRef {
        /// Optional sheet qualifier.
        sheet: Option<String>,
        /// First corner coordinate.
        first: CellAddr,
        /// Second corner coordinate.
        last: CellAddr,
    },
    /// Full-column range (`A:C`, `$A:$C`).
    ColRange {
        /// Optional sheet qualifier.
        sheet: Option<String>,
        /// First 0-based column.
        first_col: u32,
        /// Whether `first_col` had `$`.
        abs_first: bool,
        /// Second 0-based column.
        last_col: u32,
        /// Whether `last_col` had `$`.
        abs_last: bool,
    },
    /// Full-row range (`1:5`, `$1:$5`).
    RowRange {
        /// Optional sheet qualifier.
        sheet: Option<String>,
        /// First 0-based row.
        first_row: u32,
        /// Whether `first_row` had `$`.
        abs_first: bool,
        /// Second 0-based row.
        last_row: u32,
        /// Whether `last_row` had `$`.
        abs_last: bool,
    },
    /// Unresolved or workbook/sheet-local named identifier.
    Name(String),
    /// Worksheet-qualified identifier (`Sheet2!TaxRate`).
    QualifiedName {
        /// Qualifier worksheet name.
        sheet: String,
        /// Identifier name.
        name: String,
    },
    /// Dynamic-array spill-range operator (`A1#`, `Sheet2!Data#`).
    SpillRef(Box<Self>),
    /// Explicit parenthesised sub-expression `(expr)` — preserved so both
    /// pretty-print round-trips and D-027 outermost `=(A1-B1)` suppression hold.
    Paren(Box<Self>),
    /// Prefix or postfix unary operation.
    Unary {
        /// Operator kind.
        op: UnaryOp,
        /// Operand expression.
        expr: Box<Self>,
    },
    /// Binary infix operation.
    Binary {
        /// Operator kind.
        op: BinaryOp,
        /// Left-hand operand.
        left: Box<Self>,
        /// Right-hand operand.
        right: Box<Self>,
    },
    /// Function call (`SUM(A1:A10)`, `TRUE()`).
    Call {
        /// Canonical uppercase function name.
        name: String,
        /// Positional argument expressions.
        args: Vec<Self>,
    },
}

impl Expr {
    /// Formats this AST as a canonical formula string beginning with `'='`.
    #[must_use]
    pub fn to_formula_string(&self) -> String {
        let mut out = String::from("=");
        self.write_expr(&mut out, None);
        out
    }

    /// Appends this expression to `out`, inserting parentheses only when
    /// child operator precedence or associativity requires them.
    pub fn write_expr(&self, out: &mut String, parent_prec: Option<(u8, bool)>) {
        match self {
            Self::Literal(lit) => write_literal(lit, out),
            Self::ArrayLiteral {
                rows,
                cols,
                elements,
            } => {
                out.push('{');
                for r in 0..*rows {
                    if r > 0 {
                        out.push(';');
                    }
                    for c in 0..*cols {
                        if c > 0 {
                            out.push(',');
                        }
                        let idx = (r as usize)
                            .saturating_mul(*cols as usize)
                            .saturating_add(c as usize);
                        if let Some(lit) = elements.get(idx) {
                            write_literal(lit, out);
                        }
                    }
                }
                out.push('}');
            }
            Self::CellRef { sheet, addr } => {
                write_sheet_prefix(sheet.as_deref(), out);
                addr.write_a1(out);
            }
            Self::AreaRef { sheet, first, last } => {
                write_sheet_prefix(sheet.as_deref(), out);
                first.write_a1(out);
                out.push(':');
                last.write_a1(out);
            }
            Self::ColRange {
                sheet,
                first_col,
                abs_first,
                last_col,
                abs_last,
            } => {
                write_sheet_prefix(sheet.as_deref(), out);
                if *abs_first {
                    out.push('$');
                }
                write_col_letters(*first_col, out);
                out.push(':');
                if *abs_last {
                    out.push('$');
                }
                write_col_letters(*last_col, out);
            }
            Self::RowRange {
                sheet,
                first_row,
                abs_first,
                last_row,
                abs_last,
            } => {
                write_sheet_prefix(sheet.as_deref(), out);
                if *abs_first {
                    out.push('$');
                }
                let r1 = u64::from(*first_row).saturating_add(1);
                let _ = write!(out, "{r1}:");
                if *abs_last {
                    out.push('$');
                }
                let r2 = u64::from(*last_row).saturating_add(1);
                let _ = write!(out, "{r2}");
            }
            Self::Name(ident) => out.push_str(ident),
            Self::QualifiedName { sheet, name } => {
                write_sheet_prefix(Some(sheet.as_str()), out);
                out.push_str(name);
            }
            Self::SpillRef(inner) => {
                let wrap = parent_prec.is_some_and(|(p, _)| 90 < p);
                if wrap {
                    out.push('(');
                }
                inner.write_expr(out, Some((90, false)));
                out.push('#');
                if wrap {
                    out.push(')');
                }
            }
            Self::Paren(inner) => {
                out.push('(');
                inner.write_expr(out, None);
                out.push(')');
            }
            Self::Unary { op, expr } => {
                let my_prec = match op {
                    UnaryOp::Percent => 80,
                    UnaryOp::Plus | UnaryOp::Neg | UnaryOp::ImplicitIntersection => 70,
                };
                let wrap = parent_prec.is_some_and(|(p, _)| my_prec < p);
                if wrap {
                    out.push('(');
                }
                match op {
                    UnaryOp::Plus => {
                        out.push('+');
                        expr.write_expr(out, Some((my_prec, false)));
                    }
                    UnaryOp::Neg => {
                        out.push('-');
                        expr.write_expr(out, Some((my_prec, false)));
                    }
                    UnaryOp::ImplicitIntersection => {
                        out.push('@');
                        expr.write_expr(out, Some((my_prec, false)));
                    }
                    UnaryOp::Percent => {
                        expr.write_expr(out, Some((my_prec, false)));
                        out.push('%');
                    }
                }
                if wrap {
                    out.push(')');
                }
            }
            Self::Binary { op, left, right } => {
                let my_prec = binary_prec(*op);
                let wrap = parent_prec
                    .is_some_and(|(p, is_right)| my_prec < p || (is_right && my_prec == p));
                if wrap {
                    out.push('(');
                }
                left.write_expr(out, Some((my_prec, false)));
                out.push_str(op.as_str());
                right.write_expr(out, Some((my_prec, true)));
                if wrap {
                    out.push(')');
                }
            }
            Self::Call { name, args } => {
                out.push_str(name);
                out.push('(');
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    arg.write_expr(out, None);
                }
                out.push(')');
            }
        }
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

/// Formats a scalar [`Literal`] in canonical formula syntax.
pub fn write_literal(lit: &Literal, out: &mut String) {
    match lit {
        Literal::Number(n) => {
            if *n < 0.0 {
                out.push('-');
                write_number(n.abs(), NumberFormat::General, out);
            } else {
                write_number(*n, NumberFormat::General, out);
            }
        }
        Literal::Text(s) => {
            out.push('"');
            for ch in s.chars() {
                if ch == '"' {
                    out.push('"');
                }
                out.push(ch);
            }
            out.push('"');
        }
        Literal::Bool(true) => out.push_str("TRUE"),
        Literal::Bool(false) => out.push_str("FALSE"),
        Literal::Error(err) => out.push_str(err.as_str()),
    }
}
