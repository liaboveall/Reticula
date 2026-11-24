//! Coordinate-to-identity formula binder and A1 unparser (`13-m1a.md` 5.4).
//!
//! At authoring time, A1 coordinates are bound to stable [`RowId`] and [`ColId`]
//! anchors on the target worksheet's axes. Inserting or deleting rows and
//! columns never rewrites [`BoundFormula`]: unparsing resolves current live
//! positions from the bound anchors and emits `#REF!` when a single-cell target
//! has been tombstoned or when an entire anchor range contains zero live rows
//! or columns.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write as _;

use reticula_core::axis::Axis;
use reticula_core::digest::Digest64;
use reticula_core::formula_store::TemplateStrip;
use reticula_core::id::{ColId, IdRun, RowId, SheetId, id_stream};
use reticula_core::ops::{
    CellEdit, CellInput, FormatPatch, FormulaMode, FormulaPayload, MAX_CELL_TEXT_UTF16,
    MAX_EDITS_PER_OP, MAX_FORMULA_CHARS, MoveCellsRewriteCtx, Op, OpError, ScalarValue, Workbook,
    WorkbookCellKey,
};
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
    /// Unresolved or workbook/sheet-local defined name.
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
    /// Worksheet-qualified defined name (`Sheet2!TaxRate`).
    ScopedName {
        /// Bound worksheet identity when resolved (`None` if missing or deleted).
        sheet: Option<SheetId>,
        /// Original worksheet name preserved when target sheet was unresolved at bind time.
        missing_sheet: Option<String>,
        /// Identifier spelling.
        name: String,
    },
    /// Dynamic-array spill-range operator (`expr#`).
    SpillRef(Box<Self>),
    /// Unsupported external/imported formula preserved verbatim without recalculating.
    Opaque {
        /// Original raw formula text.
        raw: String,
    },
    /// Unsupported external/imported formula whose cached value from XLSX is preserved.
    PreservedCached {
        /// Original raw formula text.
        raw: String,
        /// Cached scalar evaluation imported from storage.
        cached: ScalarValue,
    },
    /// Omitted optional argument in a function call (`XLOOKUP(A1,B1:B5,C1:C5,,0)`).
    MissingArg,
}

/// Complete bound formula stored in a workbook cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoundFormula {
    /// Dynamic vs legacy evaluation mode (D-028).
    pub mode: FormulaMode,
    /// Root bound expression.
    pub root: BoundExpr,
}

/// Row or column identity target inside a [`TemplateFormula`] (`14-m1b.md` 5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AxisTarget {
    /// Absolute reference (`$V$1`) or fixed identity.
    Fixed(u64),
    /// Signed identity delta relative to the formula cell's own `RowId` or `ColId`.
    IdDelta(i64),
}

impl AxisTarget {
    /// Resolves this target against the formula cell's own `self_id`.
    #[must_use]
    pub const fn resolve(self, self_id: u64) -> u64 {
        match self {
            Self::Fixed(id) => id,
            Self::IdDelta(delta) => self_id.wrapping_add_signed(delta),
        }
    }

    fn from_bound(target_id: u64, self_id: u64, is_abs: bool) -> Self {
        if is_abs {
            return Self::Fixed(target_id);
        }
        let diff = i128::from(target_id).saturating_sub(i128::from(self_id));
        match i64::try_from(diff) {
            Ok(delta) => Self::IdDelta(delta),
            Err(_) => Self::Fixed(target_id),
        }
    }
}

/// Relative-ID cell or range reference inside a [`TemplateFormula`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemplateRef {
    /// Single-cell template reference.
    Cell {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name, if any.
        missing_sheet: Option<String>,
        /// Target row rule.
        row: AxisTarget,
        /// Target column rule.
        col: AxisTarget,
        /// Original `$` flag on column.
        abs_col: bool,
        /// Original `$` flag on row.
        abs_row: bool,
    },
    /// Rectangular range template reference.
    Area {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name, if any.
        missing_sheet: Option<String>,
        /// Top row rule.
        first_row: AxisTarget,
        /// Left column rule.
        first_col: AxisTarget,
        /// Bottom row rule.
        last_row: AxisTarget,
        /// Right column rule.
        last_col: AxisTarget,
        /// Original `$` flag on `first_col`.
        abs_first_col: bool,
        /// Original `$` flag on `first_row`.
        abs_first_row: bool,
        /// Original `$` flag on `last_col`.
        abs_last_col: bool,
        /// Original `$` flag on `last_row`.
        abs_last_row: bool,
    },
    /// Full-column range template reference.
    ColRange {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name, if any.
        missing_sheet: Option<String>,
        /// Left column rule.
        first_col: AxisTarget,
        /// Right column rule.
        last_col: AxisTarget,
        /// Original `$` flag on `first_col`.
        abs_first: bool,
        /// Original `$` flag on `last_col`.
        abs_last: bool,
    },
    /// Full-row range template reference.
    RowRange {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name, if any.
        missing_sheet: Option<String>,
        /// Top row rule.
        first_row: AxisTarget,
        /// Bottom row rule.
        last_row: AxisTarget,
        /// Original `$` flag on `first_row`.
        abs_first: bool,
        /// Original `$` flag on `last_row`.
        abs_last: bool,
    },
    /// Out-of-bounds or invalid reference (`#REF!`).
    Invalid {
        /// Optional sheet name prefix retained for display.
        sheet_name: Option<String>,
    },
}

/// Expression tree carried inside [`TemplateFormula`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TemplateExpr {
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
    /// Template cell or range reference.
    Reference(TemplateRef),
    /// Unresolved or workbook/sheet-local defined name.
    Name(String),
    /// Explicit parentheses `(inner)`.
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
    /// Function call.
    Call {
        /// Uppercase function name.
        name: String,
        /// Argument expressions.
        args: Vec<Self>,
    },
    /// Worksheet-qualified defined name (`Sheet2!TaxRate`).
    ScopedName {
        /// Bound worksheet identity when resolved (`None` if missing or deleted).
        sheet: Option<SheetId>,
        /// Original worksheet name preserved when unresolved at bind time.
        missing_sheet: Option<String>,
        /// Identifier spelling.
        name: String,
    },
    /// Dynamic-array spill-range operator (`expr#`).
    SpillRef(Box<Self>),
    /// Unsupported external formula preserved verbatim.
    Opaque {
        /// Original raw formula text.
        raw: String,
    },
    /// Unsupported external formula with preserved cached value.
    PreservedCached {
        /// Original raw formula text.
        raw: String,
        /// Cached scalar value.
        cached: ScalarValue,
    },
    /// Omitted optional argument in a function call.
    MissingArg,
}

/// Compact formula template carried by [`Op::FillFormula`] (`14-m1b.md` 5.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemplateFormula {
    /// Dynamic vs legacy evaluation mode.
    pub mode: FormulaMode,
    /// Root template expression.
    pub root: TemplateExpr,
}

/// Identity-affine row rule inside a 1D vertical [`TemplateStrip`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AffineRow {
    /// Target row identity at strip offset `0`.
    pub base: u64,
    /// Vertical identity step per strip row offset: `0` (fixed) or `1` (sliding).
    pub stride: u8,
}

impl AffineRow {
    /// Resolves the target row identity for `0`-based strip offset `offset`.
    #[must_use]
    pub const fn resolve(self, offset: u32) -> RowId {
        RowId(
            self.base
                .saturating_add((self.stride as u64).saturating_mul(offset as u64)),
        )
    }

    fn try_pair(r0: RowId, r1: RowId) -> Option<Self> {
        if r0 == r1 {
            Some(Self {
                base: r0.0,
                stride: 0,
            })
        } else if id_stream(r0.0) == id_stream(r1.0) && r1.0 == r0.0.saturating_add(1) {
            Some(Self {
                base: r0.0,
                stride: 1,
            })
        } else {
            None
        }
    }

    fn matches(self, row: RowId, offset: u32) -> bool {
        if self.stride == 0 {
            row.0 == self.base
        } else {
            id_stream(row.0) == id_stream(self.base)
                && row.0 == self.base.saturating_add(u64::from(offset))
        }
    }

    const fn shifted(self, offset: u32) -> Self {
        Self {
            base: self.resolve(offset).0,
            stride: self.stride,
        }
    }

    fn can_merge(self, left_len: u32, right: Self) -> bool {
        self.stride == right.stride
            && if self.stride == 0 {
                self.base == right.base
            } else {
                id_stream(self.base) == id_stream(right.base)
                    && self.base.saturating_add(u64::from(left_len)) == right.base
            }
    }

    fn validate(self, len: u32) -> Result<(), OpError> {
        if self.stride > 1 {
            return Err(OpError::InvalidFormula);
        }
        if self.stride == 1 && len > 0 && IdRun::new(self.base, len).is_none() {
            return Err(OpError::InvalidFormula);
        }
        Ok(())
    }
}

/// Identity-affine cell or range reference inside [`AffineFormula`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AffineRef {
    /// Single cell anchored to `(row(offset), col)`.
    Cell {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name if missing at bind time.
        missing_sheet: Option<String>,
        /// Identity-affine row anchor.
        row: AffineRow,
        /// Fixed column identity.
        col: ColId,
        /// Original `$` flag on column.
        abs_col: bool,
        /// Original `$` flag on row.
        abs_row: bool,
    },
    /// Rectangular range anchored by inclusive corner identities.
    Area {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name if missing at bind time.
        missing_sheet: Option<String>,
        /// Identity-affine top row anchor.
        first_row: AffineRow,
        /// Fixed left column anchor.
        first_col: ColId,
        /// Identity-affine bottom row anchor.
        last_row: AffineRow,
        /// Fixed right column anchor.
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
    /// Full-column range anchored by fixed inclusive columns.
    ColRange {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name if missing at bind time.
        missing_sheet: Option<String>,
        /// Fixed left column anchor.
        first_col: ColId,
        /// Fixed right column anchor.
        last_col: ColId,
        /// Original `$` flag on `first_col`.
        abs_first: bool,
        /// Original `$` flag on `last_col`.
        abs_last: bool,
    },
    /// Full-row range anchored by identity-affine inclusive rows.
    RowRange {
        /// Target worksheet (`None` means the formula's own worksheet).
        sheet: Option<SheetId>,
        /// Unresolved external sheet name if missing at bind time.
        missing_sheet: Option<String>,
        /// Identity-affine top row anchor.
        first_row: AffineRow,
        /// Identity-affine bottom row anchor.
        last_row: AffineRow,
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

/// Identity-affine expression tree compressed across a 1D vertical strip.
#[derive(Clone, Debug, PartialEq)]
pub enum AffineExpr {
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
    /// Identity-affine cell or range reference.
    Reference(AffineRef),
    /// Unresolved or workbook/sheet-local defined name.
    Name(String),
    /// Explicit parentheses `(inner)`.
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
    /// Function call.
    Call {
        /// Uppercase function name.
        name: String,
        /// Argument expressions.
        args: Vec<Self>,
    },
    /// Worksheet-qualified defined name.
    ScopedName {
        /// Bound worksheet identity when resolved.
        sheet: Option<SheetId>,
        /// Original worksheet name preserved when target sheet was unresolved.
        missing_sheet: Option<String>,
        /// Identifier spelling.
        name: String,
    },
    /// Dynamic-array spill-range operator (`expr#`).
    SpillRef(Box<Self>),
    /// Omitted optional argument in a function call.
    MissingArg,
}

/// Identity-affine strip template carried by [`reticula_core::formula_store::FormulaStore`].
#[derive(Clone, Debug, PartialEq)]
pub struct AffineFormula {
    /// Dynamic vs legacy evaluation mode.
    pub mode: FormulaMode,
    /// Root affine expression.
    pub root: AffineExpr,
}

impl FormulaPayload for BoundFormula {
    type Template = TemplateFormula;
    type StripTemplate = AffineFormula;

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

    fn validate_template(template: &Self::Template) -> Result<usize, OpError> {
        validate_template_expr(&template.root, 0)?;
        Ok(size_of::<Self>().saturating_add(template_expanded_bytes(&template.root)))
    }

    fn expand_template(template: &Self::Template, row: RowId, col: ColId) -> Self {
        Self {
            mode: template.mode,
            root: expand_template_expr(&template.root, row, col),
        }
    }

    fn try_pair_strip_template(f0: &Self, f1: &Self) -> Option<Self::StripTemplate> {
        if f0.mode != f1.mode {
            return None;
        }
        let root = try_pair_affine_expr(&f0.root, &f1.root)?;
        Some(AffineFormula {
            mode: f0.mode,
            root,
        })
    }

    fn matches_strip_template(&self, template: &Self::StripTemplate, offset: u32) -> bool {
        self.mode == template.mode && affine_expr_matches(&self.root, &template.root, offset)
    }

    fn instantiate_strip_template(template: &Self::StripTemplate, offset: u32) -> Self {
        Self {
            mode: template.mode,
            root: instantiate_affine_expr(&template.root, offset),
        }
    }

    fn shift_strip_template(template: &Self::StripTemplate, offset: u32) -> Self::StripTemplate {
        AffineFormula {
            mode: template.mode,
            root: shift_affine_expr(&template.root, offset),
        }
    }

    fn strips_can_merge(
        left: &Self::StripTemplate,
        left_len: u32,
        right: &Self::StripTemplate,
    ) -> bool {
        left.mode == right.mode && affine_expr_can_merge(&left.root, left_len, &right.root)
    }

    fn validate_strip_template(template: &Self::StripTemplate, len: u32) -> Result<(), OpError> {
        validate_affine_expr(&template.root, 0, len)
    }

    fn strip_template_bytes(template: &Self::StripTemplate) -> usize {
        size_of::<AffineFormula>().saturating_add(affine_expr_bytes(&template.root))
    }

    fn write_strip_cell_digest(template: &Self::StripTemplate, offset: u32, digest: &mut Digest64) {
        let mode_byte = match template.mode {
            FormulaMode::Dynamic => 1_u8,
            FormulaMode::Legacy => 2_u8,
        };
        digest.write_bytes(&[mode_byte]);
        write_affine_expr_digest(&template.root, offset, digest);
    }

    fn collect_strip_row_endpoints(
        strip: &TemplateStrip<Self::StripTemplate>,
        target_sheet: SheetId,
        ids: &[IdRun],
        out: &mut BTreeSet<WorkbookCellKey>,
    ) {
        collect_affine_row_endpoints(&strip.template.root, strip, target_sheet, ids, out);
    }

    fn collect_strip_col_endpoints(
        strip: &TemplateStrip<Self::StripTemplate>,
        target_sheet: SheetId,
        ids: &[IdRun],
        out: &mut BTreeSet<WorkbookCellKey>,
    ) {
        if affine_expr_touches_col_endpoints(&strip.template.root, strip.sheet, target_sheet, ids) {
            for offset in 0..strip.len {
                let row = RowId(strip.start_row.0.saturating_add(u64::from(offset)));
                out.insert((strip.sheet, row, strip.col));
            }
        }
    }

    fn collect_range_endpoints(
        &self,
        host_sheet: SheetId,
        row_endpoints: &mut Vec<(SheetId, RowId)>,
        col_endpoints: &mut Vec<(SheetId, ColId)>,
    ) {
        collect_expr_range_endpoints(&self.root, host_sheet, row_endpoints, col_endpoints);
    }

    fn handoff_row_anchors(
        &mut self,
        host_sheet: SheetId,
        moved_sheet: SheetId,
        row_axis: &Axis,
        moved: &[IdRun],
        after: Option<RowId>,
    ) -> bool {
        handoff_expr_rows(
            &mut self.root,
            host_sheet,
            moved_sheet,
            row_axis,
            moved,
            after.map(|r| r.0),
        )
    }

    fn handoff_col_anchors(
        &mut self,
        host_sheet: SheetId,
        moved_sheet: SheetId,
        col_axis: &Axis,
        moved: &[IdRun],
        after: Option<ColId>,
    ) -> bool {
        handoff_expr_cols(
            &mut self.root,
            host_sheet,
            moved_sheet,
            col_axis,
            moved,
            after.map(|c| c.0),
        )
    }

    fn finalize_purged_tombstones(&mut self, host_sheet: SheetId, sheets: &[Sheet]) -> bool {
        finalize_expr_tombstones(&mut self.root, host_sheet, sheets)
    }

    fn shift_for_row_permutation(
        &self,
        sheet: SheetId,
        src_row_pos: u32,
        dst_row_pos: u32,
        sheets: &[Sheet],
    ) -> Self {
        if src_row_pos == dst_row_pos {
            return self.clone();
        }
        let dr = i64::from(dst_row_pos).saturating_sub(i64::from(src_row_pos));
        let find_sheet = |id: SheetId| -> Option<&Sheet> { sheets.iter().find(|s| s.id() == id) };
        Self {
            mode: self.mode,
            root: shift_bound_expr_with(&self.root, &find_sheet, sheet, sheet, dr, 0),
        }
    }

    fn rewrite_for_move_cells(
        &mut self,
        host_sheet_before: SheetId,
        is_moved_formula: bool,
        ctx: &MoveCellsRewriteCtx<'_>,
    ) -> bool {
        let Some(plan) = MoveCellsPlan::from_ctx(ctx) else {
            return false;
        };
        rewrite_bound_expr_for_move_cells(
            &mut self.root,
            host_sheet_before,
            is_moved_formula,
            &plan,
        )
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

    /// Whether this formula invokes volatile functions (`TODAY`, `NOW`, `RAND`,
    /// or `RANDBETWEEN`; `14-m1b.md` 5.3).
    #[must_use]
    pub fn is_volatile(&self) -> bool {
        expr_is_volatile(&self.root)
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

    /// Rewrites any bound sheet/row/column identity present in `sheets`/`rows`/`cols`
    /// (used when remapping offline-created formulas after a sync epoch or tail-loss bump).
    pub fn remap_ids(
        &mut self,
        sheets: &BTreeMap<SheetId, SheetId>,
        rows: &BTreeMap<RowId, RowId>,
        cols: &BTreeMap<ColId, ColId>,
    ) {
        if sheets.is_empty() && rows.is_empty() && cols.is_empty() {
            return;
        }
        remap_bound_expr(&mut self.root, sheets, rows, cols);
    }
}

impl TemplateFormula {
    /// Rewrites any bound sheet or fixed row/column identity present in
    /// `sheets`/`rows`/`cols` (`IdDelta` offsets stay unchanged).
    pub fn remap_ids(
        &mut self,
        sheets: &BTreeMap<SheetId, SheetId>,
        rows: &BTreeMap<RowId, RowId>,
        cols: &BTreeMap<ColId, ColId>,
    ) {
        if sheets.is_empty() && rows.is_empty() && cols.is_empty() {
            return;
        }
        remap_template_expr(&mut self.root, sheets, rows, cols);
    }
}

fn remap_opt_sheet(sheet: &mut Option<SheetId>, sheets: &BTreeMap<SheetId, SheetId>) {
    if let Some(s) = sheet
        && let Some(&mapped) = sheets.get(s)
    {
        *s = mapped;
    }
}

fn remap_row(row: &mut RowId, rows: &BTreeMap<RowId, RowId>) {
    if let Some(&mapped) = rows.get(row) {
        *row = mapped;
    }
}

fn remap_col(col: &mut ColId, cols: &BTreeMap<ColId, ColId>) {
    if let Some(&mapped) = cols.get(col) {
        *col = mapped;
    }
}

fn remap_bound_expr(
    expr: &mut BoundExpr,
    sheets: &BTreeMap<SheetId, SheetId>,
    rows: &BTreeMap<RowId, RowId>,
    cols: &BTreeMap<ColId, ColId>,
) {
    match expr {
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Name(_)
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => {}
        BoundExpr::ScopedName { sheet, .. } => {
            remap_opt_sheet(sheet, sheets);
        }
        BoundExpr::Reference(r) => match r {
            BoundRef::Cell {
                sheet, row, col, ..
            } => {
                remap_opt_sheet(sheet, sheets);
                remap_row(row, rows);
                remap_col(col, cols);
            }
            BoundRef::Area {
                sheet,
                first_row,
                first_col,
                last_row,
                last_col,
                ..
            } => {
                remap_opt_sheet(sheet, sheets);
                remap_row(first_row, rows);
                remap_col(first_col, cols);
                remap_row(last_row, rows);
                remap_col(last_col, cols);
            }
            BoundRef::ColRange {
                sheet,
                first_col,
                last_col,
                ..
            } => {
                remap_opt_sheet(sheet, sheets);
                remap_col(first_col, cols);
                remap_col(last_col, cols);
            }
            BoundRef::RowRange {
                sheet,
                first_row,
                last_row,
                ..
            } => {
                remap_opt_sheet(sheet, sheets);
                remap_row(first_row, rows);
                remap_row(last_row, rows);
            }
            BoundRef::Invalid { .. } => {}
        },
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => {
            remap_bound_expr(inner, sheets, rows, cols);
        }
        BoundExpr::Binary { left, right, .. } => {
            remap_bound_expr(left, sheets, rows, cols);
            remap_bound_expr(right, sheets, rows, cols);
        }
        BoundExpr::Call { args, .. } => {
            for arg in args {
                remap_bound_expr(arg, sheets, rows, cols);
            }
        }
    }
}

fn remap_template_expr(
    expr: &mut TemplateExpr,
    sheets: &BTreeMap<SheetId, SheetId>,
    rows: &BTreeMap<RowId, RowId>,
    cols: &BTreeMap<ColId, ColId>,
) {
    match expr {
        TemplateExpr::Literal(_)
        | TemplateExpr::ArrayLiteral { .. }
        | TemplateExpr::Name(_)
        | TemplateExpr::Opaque { .. }
        | TemplateExpr::PreservedCached { .. }
        | TemplateExpr::MissingArg => {}
        TemplateExpr::ScopedName { sheet, .. } => {
            remap_opt_sheet(sheet, sheets);
        }
        TemplateExpr::Reference(r) => match r {
            TemplateRef::Cell {
                sheet, row, col, ..
            } => {
                remap_opt_sheet(sheet, sheets);
                if let AxisTarget::Fixed(id) = row
                    && let Some(&mapped) = rows.get(&RowId(*id))
                {
                    *id = mapped.0;
                }
                if let AxisTarget::Fixed(id) = col
                    && let Some(&mapped) = cols.get(&ColId(*id))
                {
                    *id = mapped.0;
                }
            }
            TemplateRef::Area {
                sheet,
                first_row,
                first_col,
                last_row,
                last_col,
                ..
            } => {
                remap_opt_sheet(sheet, sheets);
                for fr in [first_row, last_row] {
                    if let AxisTarget::Fixed(id) = fr
                        && let Some(&mapped) = rows.get(&RowId(*id))
                    {
                        *id = mapped.0;
                    }
                }
                for fc in [first_col, last_col] {
                    if let AxisTarget::Fixed(id) = fc
                        && let Some(&mapped) = cols.get(&ColId(*id))
                    {
                        *id = mapped.0;
                    }
                }
            }
            TemplateRef::ColRange {
                sheet,
                first_col,
                last_col,
                ..
            } => {
                remap_opt_sheet(sheet, sheets);
                for fc in [first_col, last_col] {
                    if let AxisTarget::Fixed(id) = fc
                        && let Some(&mapped) = cols.get(&ColId(*id))
                    {
                        *id = mapped.0;
                    }
                }
            }
            TemplateRef::RowRange {
                sheet,
                first_row,
                last_row,
                ..
            } => {
                remap_opt_sheet(sheet, sheets);
                for fr in [first_row, last_row] {
                    if let AxisTarget::Fixed(id) = fr
                        && let Some(&mapped) = rows.get(&RowId(*id))
                    {
                        *id = mapped.0;
                    }
                }
            }
            TemplateRef::Invalid { .. } => {}
        },
        TemplateExpr::Paren(inner)
        | TemplateExpr::Unary { expr: inner, .. }
        | TemplateExpr::SpillRef(inner) => {
            remap_template_expr(inner, sheets, rows, cols);
        }
        TemplateExpr::Binary { left, right, .. } => {
            remap_template_expr(left, sheets, rows, cols);
            remap_template_expr(right, sheets, rows, cols);
        }
        TemplateExpr::Call { args, .. } => {
            for arg in args {
                remap_template_expr(arg, sheets, rows, cols);
            }
        }
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
        Expr::QualifiedName { sheet, name } => {
            match resolve_target_sheet(Some(sheet.as_str()), workbook, host_sheet) {
                Some((Some(sid), _)) => BoundExpr::ScopedName {
                    sheet: Some(sid),
                    missing_sheet: None,
                    name: name.clone(),
                },
                Some((None, _)) => BoundExpr::ScopedName {
                    sheet: Some(host_sheet),
                    missing_sheet: None,
                    name: name.clone(),
                },
                None => BoundExpr::ScopedName {
                    sheet: None,
                    missing_sheet: Some(sheet.clone()),
                    name: name.clone(),
                },
            }
        }
        Expr::SpillRef(inner) => {
            BoundExpr::SpillRef(Box::new(bind_expr(inner, workbook, host_sheet)))
        }
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
        Expr::MissingArg => BoundExpr::MissingArg,
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
        Expr::QualifiedName { sheet: s, name } => BoundExpr::ScopedName {
            sheet: None,
            missing_sheet: Some(s.clone()),
            name: name.clone(),
        },
        Expr::SpillRef(inner) => {
            BoundExpr::SpillRef(Box::new(bind_expr_single_sheet(inner, sheet)))
        }
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
        Expr::MissingArg => BoundExpr::MissingArg,
    }
}

fn resolve_target_sheet<'a>(
    sheet_name: Option<&str>,
    workbook: &'a Workbook<BoundFormula>,
    host_sheet: SheetId,
) -> Option<(Option<SheetId>, &'a Sheet)> {
    match sheet_name {
        None => workbook.sheet(host_sheet).map(|s| (None, s)),
        Some(name) if name.eq_ignore_ascii_case("#REF!") => None,
        Some(name) => workbook
            .live_sheets()
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
        BoundExpr::ScopedName {
            sheet,
            missing_sheet,
            name,
        } => match sheet {
            Some(id) => match workbook.sheet(*id) {
                Some(s) if !s.is_deleted() => {
                    write_sheet_prefix(Some(s.name()), out);
                    out.push_str(name);
                }
                _ => out.push_str("#REF!"),
            },
            None => {
                if let Some(ms) = missing_sheet {
                    write_sheet_prefix(Some(ms), out);
                }
                out.push_str(name);
            }
        },
        BoundExpr::SpillRef(inner) => {
            let wrap = parent_prec.is_some_and(|(p, _)| 90 < p);
            if wrap {
                out.push('(');
            }
            unparse_expr(inner, workbook, host_sheet, out, Some((90, false)));
            out.push('#');
            if wrap {
                out.push(')');
            }
        }
        BoundExpr::Opaque { raw } | BoundExpr::PreservedCached { raw, .. } => {
            out.push_str(raw.strip_prefix('=').unwrap_or(raw));
        }
        BoundExpr::MissingArg => {}
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
        BoundExpr::Reference(r) => unparse_ref_on_sheet(r, SheetPrefix::None, Some(sheet), out),
        BoundExpr::Name(name) => out.push_str(name),
        BoundExpr::ScopedName {
            sheet: target_id,
            missing_sheet,
            name,
        } => {
            if let Some(ms) = missing_sheet {
                write_sheet_prefix(Some(ms), out);
                out.push_str(name);
            } else if target_id.is_some_and(|id| id == sheet.id()) && !sheet.is_deleted() {
                write_sheet_prefix(Some(sheet.name()), out);
                out.push_str(name);
            } else {
                out.push_str("#REF!");
            }
        }
        BoundExpr::SpillRef(inner) => {
            let wrap = parent_prec.is_some_and(|(p, _)| 90 < p);
            if wrap {
                out.push('(');
            }
            unparse_expr_single_sheet(inner, sheet, out, Some((90, false)));
            out.push('#');
            if wrap {
                out.push(')');
            }
        }
        BoundExpr::Opaque { raw } | BoundExpr::PreservedCached { raw, .. } => {
            out.push_str(raw.strip_prefix('=').unwrap_or(raw));
        }
        BoundExpr::MissingArg => {}
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

#[derive(Clone, Copy)]
enum SheetPrefix<'a> {
    None,
    Live(&'a str),
    DeletedRef,
}

impl SheetPrefix<'_> {
    fn write_to(self, out: &mut String) {
        match self {
            Self::None => {}
            Self::Live(name) => write_sheet_prefix(Some(name), out),
            Self::DeletedRef => out.push_str("#REF!"),
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
        } => match workbook.sheet(*id) {
            Some(s) if s.is_deleted() => SheetPrefix::DeletedRef,
            Some(s) => SheetPrefix::Live(s.name()),
            None => SheetPrefix::DeletedRef,
        },
        _ => SheetPrefix::None,
    };
    unparse_ref_on_sheet(bound, prefix, target, out);
}

fn unparse_ref_on_sheet(
    bound: &BoundRef,
    prefix: SheetPrefix<'_>,
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
            prefix.write_to(out);
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
            prefix.write_to(out);
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
            prefix.write_to(out);
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
            prefix.write_to(out);
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
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => false,
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => expr_is_structure_sensitive(inner),
        BoundExpr::Binary { left, right, .. } => {
            expr_is_structure_sensitive(left) || expr_is_structure_sensitive(right)
        }
        BoundExpr::Call { name, args } => {
            name == "ROW" || name == "COLUMN" || args.iter().any(expr_is_structure_sensitive)
        }
    }
}

fn expr_is_volatile(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Reference(_)
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => false,
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => expr_is_volatile(inner),
        BoundExpr::Binary { left, right, .. } => expr_is_volatile(left) || expr_is_volatile(right),
        BoundExpr::Call { name, args } => {
            crate::functions::catalog::is_volatile_function(name)
                || args.iter().any(expr_is_volatile)
        }
    }
}

fn expr_bytes(expr: &BoundExpr) -> usize {
    let base = size_of::<BoundExpr>();
    match expr {
        BoundExpr::Literal(Literal::Text(s))
        | BoundExpr::Name(s)
        | BoundExpr::Opaque { raw: s } => base.saturating_add(s.capacity()),
        BoundExpr::ScopedName {
            missing_sheet,
            name,
            ..
        } => base
            .saturating_add(name.capacity())
            .saturating_add(missing_sheet.as_ref().map_or(0, String::capacity)),
        BoundExpr::PreservedCached { raw, cached } => {
            let text_cap = match cached {
                ScalarValue::Text(s) => s.capacity(),
                _ => 0,
            };
            base.saturating_add(raw.capacity()).saturating_add(text_cap)
        }
        BoundExpr::Literal(_) | BoundExpr::Reference(_) | BoundExpr::MissingArg => base,
        BoundExpr::ArrayLiteral { elements, .. } => {
            base.saturating_add(elements.capacity().saturating_mul(size_of::<Literal>()))
        }
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => base.saturating_add(expr_bytes(inner)),
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
        BoundExpr::Literal(_)
        | BoundExpr::Reference(_)
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { .. }
        | BoundExpr::MissingArg => Ok(()),
        BoundExpr::Opaque { raw } => {
            if raw.chars().count() > MAX_FORMULA_CHARS {
                Err(OpError::FormulaTooLong)
            } else {
                Ok(())
            }
        }
        BoundExpr::PreservedCached { raw, cached } => {
            if raw.chars().count() > MAX_FORMULA_CHARS {
                return Err(OpError::FormulaTooLong);
            }
            match cached {
                ScalarValue::Number(n) if !n.is_finite() => Err(OpError::NonFiniteNumber),
                ScalarValue::Text(s) if s.encode_utf16().count() > MAX_CELL_TEXT_UTF16 => {
                    Err(OpError::TextTooLong)
                }
                _ => Ok(()),
            }
        }
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
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => validate_bound_expr(inner, depth.saturating_add(1)),
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
        BoundExpr::SpillRef(inner) => {
            d.write_bytes(&[9]);
            write_expr_digest(inner, d);
        }
        BoundExpr::ScopedName {
            sheet,
            missing_sheet,
            name,
        } => {
            d.write_bytes(&[10]);
            match sheet {
                None => d.write_bytes(&[0]),
                Some(sid) => {
                    d.write_bytes(&[1]);
                    d.write_u32(sid.0);
                }
            }
            match missing_sheet.as_deref() {
                None => d.write_bytes(&[0]),
                Some(ms) => {
                    d.write_bytes(&[1]);
                    d.write_u32(u32::try_from(ms.len()).unwrap_or(u32::MAX));
                    d.write_bytes(ms.as_bytes());
                }
            }
            d.write_u32(u32::try_from(name.len()).unwrap_or(u32::MAX));
            d.write_bytes(name.as_bytes());
        }
        BoundExpr::Opaque { raw } => {
            d.write_bytes(&[11]);
            d.write_u32(u32::try_from(raw.len()).unwrap_or(u32::MAX));
            d.write_bytes(raw.as_bytes());
        }
        BoundExpr::PreservedCached { raw, cached } => {
            d.write_bytes(&[12]);
            d.write_u32(u32::try_from(raw.len()).unwrap_or(u32::MAX));
            d.write_bytes(raw.as_bytes());
            match cached {
                ScalarValue::Number(n) => {
                    d.write_bytes(&[1]);
                    d.write_u64(if *n == 0.0 {
                        0.0_f64.to_bits()
                    } else {
                        n.to_bits()
                    });
                }
                ScalarValue::Text(s) => {
                    d.write_bytes(&[2]);
                    d.write_u32(u32::try_from(s.len()).unwrap_or(u32::MAX));
                    d.write_bytes(s.as_bytes());
                }
                ScalarValue::Bool(b) => d.write_bytes(&[3, u8::from(*b)]),
                ScalarValue::Error(e) => {
                    d.write_bytes(&[4]);
                    d.write_bytes(e.as_str().as_bytes());
                }
            }
        }
        BoundExpr::MissingArg => d.write_bytes(&[13]),
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

fn lit_bits_eq(a: &Literal, b: &Literal) -> bool {
    match (a, b) {
        (Literal::Number(x), Literal::Number(y)) => x.to_bits() == y.to_bits(),
        (Literal::Text(x), Literal::Text(y)) => x == y,
        (Literal::Bool(x), Literal::Bool(y)) => x == y,
        (Literal::Error(x), Literal::Error(y)) => x == y,
        _ => false,
    }
}

fn lits_bits_eq(a: &[Literal], b: &[Literal]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| lit_bits_eq(x, y))
}

fn try_pair_affine_ref(r0: &BoundRef, r1: &BoundRef) -> Option<AffineRef> {
    match (r0, r1) {
        (
            BoundRef::Cell {
                sheet: s0,
                missing_sheet: ms0,
                row: row0,
                col: col0,
                abs_col: ac0,
                abs_row: ar0,
            },
            BoundRef::Cell {
                sheet: s1,
                missing_sheet: ms1,
                row: row1,
                col: col1,
                abs_col: ac1,
                abs_row: ar1,
            },
        ) if s0 == s1 && ms0 == ms1 && col0 == col1 && ac0 == ac1 && ar0 == ar1 => {
            Some(AffineRef::Cell {
                sheet: *s0,
                missing_sheet: ms0.clone(),
                row: AffineRow::try_pair(*row0, *row1)?,
                col: *col0,
                abs_col: *ac0,
                abs_row: *ar0,
            })
        }
        (
            BoundRef::Area {
                sheet: s0,
                missing_sheet: ms0,
                first_row: fr0,
                first_col: fc0,
                last_row: lr0,
                last_col: lc0,
                abs_first_col: afc0,
                abs_first_row: afr0,
                abs_last_col: alc0,
                abs_last_row: alr0,
            },
            BoundRef::Area {
                sheet: s1,
                missing_sheet: ms1,
                first_row: fr1,
                first_col: fc1,
                last_row: lr1,
                last_col: lc1,
                abs_first_col: afc1,
                abs_first_row: afr1,
                abs_last_col: alc1,
                abs_last_row: alr1,
            },
        ) if s0 == s1
            && ms0 == ms1
            && fc0 == fc1
            && lc0 == lc1
            && afc0 == afc1
            && afr0 == afr1
            && alc0 == alc1
            && alr0 == alr1 =>
        {
            Some(AffineRef::Area {
                sheet: *s0,
                missing_sheet: ms0.clone(),
                first_row: AffineRow::try_pair(*fr0, *fr1)?,
                first_col: *fc0,
                last_row: AffineRow::try_pair(*lr0, *lr1)?,
                last_col: *lc0,
                abs_first_col: *afc0,
                abs_first_row: *afr0,
                abs_last_col: *alc0,
                abs_last_row: *alr0,
            })
        }
        (
            BoundRef::ColRange {
                sheet: s0,
                missing_sheet: ms0,
                first_col: fc0,
                last_col: lc0,
                abs_first: af0,
                abs_last: al0,
            },
            BoundRef::ColRange {
                sheet: s1,
                missing_sheet: ms1,
                first_col: fc1,
                last_col: lc1,
                abs_first: af1,
                abs_last: al1,
            },
        ) if s0 == s1 && ms0 == ms1 && fc0 == fc1 && lc0 == lc1 && af0 == af1 && al0 == al1 => {
            Some(AffineRef::ColRange {
                sheet: *s0,
                missing_sheet: ms0.clone(),
                first_col: *fc0,
                last_col: *lc0,
                abs_first: *af0,
                abs_last: *al0,
            })
        }
        (
            BoundRef::RowRange {
                sheet: s0,
                missing_sheet: ms0,
                first_row: fr0,
                last_row: lr0,
                abs_first: af0,
                abs_last: al0,
            },
            BoundRef::RowRange {
                sheet: s1,
                missing_sheet: ms1,
                first_row: fr1,
                last_row: lr1,
                abs_first: af1,
                abs_last: al1,
            },
        ) if s0 == s1 && ms0 == ms1 && af0 == af1 && al0 == al1 => Some(AffineRef::RowRange {
            sheet: *s0,
            missing_sheet: ms0.clone(),
            first_row: AffineRow::try_pair(*fr0, *fr1)?,
            last_row: AffineRow::try_pair(*lr0, *lr1)?,
            abs_first: *af0,
            abs_last: *al0,
        }),
        (BoundRef::Invalid { sheet_name: n0 }, BoundRef::Invalid { sheet_name: n1 })
            if n0 == n1 =>
        {
            Some(AffineRef::Invalid {
                sheet_name: n0.clone(),
            })
        }
        _ => None,
    }
}

fn try_pair_affine_expr(e0: &BoundExpr, e1: &BoundExpr) -> Option<AffineExpr> {
    match (e0, e1) {
        (BoundExpr::Literal(a), BoundExpr::Literal(b)) if lit_bits_eq(a, b) => {
            Some(AffineExpr::Literal(a.clone()))
        }
        (
            BoundExpr::ArrayLiteral {
                rows: r0,
                cols: c0,
                elements: el0,
            },
            BoundExpr::ArrayLiteral {
                rows: r1,
                cols: c1,
                elements: el1,
            },
        ) if r0 == r1 && c0 == c1 && lits_bits_eq(el0, el1) => Some(AffineExpr::ArrayLiteral {
            rows: *r0,
            cols: *c0,
            elements: el0.clone(),
        }),
        (BoundExpr::Reference(r0), BoundExpr::Reference(r1)) => {
            Some(AffineExpr::Reference(try_pair_affine_ref(r0, r1)?))
        }
        (BoundExpr::Name(a), BoundExpr::Name(b)) if a == b => Some(AffineExpr::Name(a.clone())),
        (BoundExpr::Paren(a), BoundExpr::Paren(b)) => {
            Some(AffineExpr::Paren(Box::new(try_pair_affine_expr(a, b)?)))
        }
        (BoundExpr::Unary { op: op0, expr: a }, BoundExpr::Unary { op: op1, expr: b })
            if op0 == op1 =>
        {
            Some(AffineExpr::Unary {
                op: *op0,
                expr: Box::new(try_pair_affine_expr(a, b)?),
            })
        }
        (
            BoundExpr::Binary {
                op: op0,
                left: l0,
                right: r0,
            },
            BoundExpr::Binary {
                op: op1,
                left: l1,
                right: r1,
            },
        ) if op0 == op1 => Some(AffineExpr::Binary {
            op: *op0,
            left: Box::new(try_pair_affine_expr(l0, l1)?),
            right: Box::new(try_pair_affine_expr(r0, r1)?),
        }),
        (BoundExpr::Call { name: n0, args: a0 }, BoundExpr::Call { name: n1, args: a1 })
            if n0 == n1 && a0.len() == a1.len() =>
        {
            let mut args = Vec::with_capacity(a0.len());
            for (x, y) in a0.iter().zip(a1) {
                args.push(try_pair_affine_expr(x, y)?);
            }
            Some(AffineExpr::Call {
                name: n0.clone(),
                args,
            })
        }
        (
            BoundExpr::ScopedName {
                sheet: s0,
                missing_sheet: ms0,
                name: n0,
            },
            BoundExpr::ScopedName {
                sheet: s1,
                missing_sheet: ms1,
                name: n1,
            },
        ) if s0 == s1 && ms0 == ms1 && n0 == n1 => Some(AffineExpr::ScopedName {
            sheet: *s0,
            missing_sheet: ms0.clone(),
            name: n0.clone(),
        }),
        (BoundExpr::SpillRef(a), BoundExpr::SpillRef(b)) => {
            Some(AffineExpr::SpillRef(Box::new(try_pair_affine_expr(a, b)?)))
        }
        (BoundExpr::MissingArg, BoundExpr::MissingArg) => Some(AffineExpr::MissingArg),
        _ => None,
    }
}

fn affine_ref_matches(r: &BoundRef, a: &AffineRef, offset: u32) -> bool {
    match (r, a) {
        (
            BoundRef::Cell {
                sheet: s0,
                missing_sheet: ms0,
                row: row0,
                col: col0,
                abs_col: ac0,
                abs_row: ar0,
            },
            AffineRef::Cell {
                sheet: s1,
                missing_sheet: ms1,
                row: row1,
                col: col1,
                abs_col: ac1,
                abs_row: ar1,
            },
        ) => {
            s0 == s1
                && ms0 == ms1
                && col0 == col1
                && ac0 == ac1
                && ar0 == ar1
                && row1.matches(*row0, offset)
        }
        (
            BoundRef::Area {
                sheet: s0,
                missing_sheet: ms0,
                first_row: fr0,
                first_col: fc0,
                last_row: lr0,
                last_col: lc0,
                abs_first_col: afc0,
                abs_first_row: afr0,
                abs_last_col: alc0,
                abs_last_row: alr0,
            },
            AffineRef::Area {
                sheet: s1,
                missing_sheet: ms1,
                first_row: fr1,
                first_col: fc1,
                last_row: lr1,
                last_col: lc1,
                abs_first_col: afc1,
                abs_first_row: afr1,
                abs_last_col: alc1,
                abs_last_row: alr1,
            },
        ) => {
            s0 == s1
                && ms0 == ms1
                && fc0 == fc1
                && lc0 == lc1
                && afc0 == afc1
                && afr0 == afr1
                && alc0 == alc1
                && alr0 == alr1
                && fr1.matches(*fr0, offset)
                && lr1.matches(*lr0, offset)
        }
        (
            BoundRef::ColRange {
                sheet: s0,
                missing_sheet: ms0,
                first_col: fc0,
                last_col: lc0,
                abs_first: af0,
                abs_last: al0,
            },
            AffineRef::ColRange {
                sheet: s1,
                missing_sheet: ms1,
                first_col: fc1,
                last_col: lc1,
                abs_first: af1,
                abs_last: al1,
            },
        ) => s0 == s1 && ms0 == ms1 && fc0 == fc1 && lc0 == lc1 && af0 == af1 && al0 == al1,
        (
            BoundRef::RowRange {
                sheet: s0,
                missing_sheet: ms0,
                first_row: fr0,
                last_row: lr0,
                abs_first: af0,
                abs_last: al0,
            },
            AffineRef::RowRange {
                sheet: s1,
                missing_sheet: ms1,
                first_row: fr1,
                last_row: lr1,
                abs_first: af1,
                abs_last: al1,
            },
        ) => {
            s0 == s1
                && ms0 == ms1
                && af0 == af1
                && al0 == al1
                && fr1.matches(*fr0, offset)
                && lr1.matches(*lr0, offset)
        }
        (BoundRef::Invalid { sheet_name: n0 }, AffineRef::Invalid { sheet_name: n1 }) => n0 == n1,
        _ => false,
    }
}

fn affine_expr_matches(e: &BoundExpr, a: &AffineExpr, offset: u32) -> bool {
    match (e, a) {
        (BoundExpr::Literal(x), AffineExpr::Literal(y)) => lit_bits_eq(x, y),
        (
            BoundExpr::ArrayLiteral {
                rows: r0,
                cols: c0,
                elements: el0,
            },
            AffineExpr::ArrayLiteral {
                rows: r1,
                cols: c1,
                elements: el1,
            },
        ) => r0 == r1 && c0 == c1 && lits_bits_eq(el0, el1),
        (BoundExpr::Reference(r0), AffineExpr::Reference(r1)) => affine_ref_matches(r0, r1, offset),
        (BoundExpr::Name(x), AffineExpr::Name(y)) => x == y,
        (BoundExpr::Paren(x), AffineExpr::Paren(y))
        | (BoundExpr::SpillRef(x), AffineExpr::SpillRef(y)) => affine_expr_matches(x, y, offset),
        (BoundExpr::Unary { op: op0, expr: x }, AffineExpr::Unary { op: op1, expr: y }) => {
            op0 == op1 && affine_expr_matches(x, y, offset)
        }
        (
            BoundExpr::Binary {
                op: op0,
                left: l0,
                right: r0,
            },
            AffineExpr::Binary {
                op: op1,
                left: l1,
                right: r1,
            },
        ) => {
            op0 == op1 && affine_expr_matches(l0, l1, offset) && affine_expr_matches(r0, r1, offset)
        }
        (BoundExpr::Call { name: n0, args: a0 }, AffineExpr::Call { name: n1, args: a1 }) => {
            n0 == n1
                && a0.len() == a1.len()
                && a0
                    .iter()
                    .zip(a1)
                    .all(|(x, y)| affine_expr_matches(x, y, offset))
        }
        (
            BoundExpr::ScopedName {
                sheet: s0,
                missing_sheet: ms0,
                name: n0,
            },
            AffineExpr::ScopedName {
                sheet: s1,
                missing_sheet: ms1,
                name: n1,
            },
        ) => s0 == s1 && ms0 == ms1 && n0 == n1,
        (BoundExpr::MissingArg, AffineExpr::MissingArg) => true,
        _ => false,
    }
}

fn instantiate_affine_ref(r: &AffineRef, offset: u32) -> BoundRef {
    match r {
        AffineRef::Cell {
            sheet,
            missing_sheet,
            row,
            col,
            abs_col,
            abs_row,
        } => BoundRef::Cell {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            row: row.resolve(offset),
            col: *col,
            abs_col: *abs_col,
            abs_row: *abs_row,
        },
        AffineRef::Area {
            sheet,
            missing_sheet,
            first_row,
            first_col,
            last_row,
            last_col,
            abs_first_col,
            abs_first_row,
            abs_last_col,
            abs_last_row,
        } => BoundRef::Area {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            first_row: first_row.resolve(offset),
            first_col: *first_col,
            last_row: last_row.resolve(offset),
            last_col: *last_col,
            abs_first_col: *abs_first_col,
            abs_first_row: *abs_first_row,
            abs_last_col: *abs_last_col,
            abs_last_row: *abs_last_row,
        },
        AffineRef::ColRange {
            sheet,
            missing_sheet,
            first_col,
            last_col,
            abs_first,
            abs_last,
        } => BoundRef::ColRange {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            first_col: *first_col,
            last_col: *last_col,
            abs_first: *abs_first,
            abs_last: *abs_last,
        },
        AffineRef::RowRange {
            sheet,
            missing_sheet,
            first_row,
            last_row,
            abs_first,
            abs_last,
        } => BoundRef::RowRange {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            first_row: first_row.resolve(offset),
            last_row: last_row.resolve(offset),
            abs_first: *abs_first,
            abs_last: *abs_last,
        },
        AffineRef::Invalid { sheet_name } => BoundRef::Invalid {
            sheet_name: sheet_name.clone(),
        },
    }
}

fn instantiate_affine_expr(expr: &AffineExpr, offset: u32) -> BoundExpr {
    match expr {
        AffineExpr::Literal(lit) => BoundExpr::Literal(lit.clone()),
        AffineExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => BoundExpr::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.clone(),
        },
        AffineExpr::Reference(r) => BoundExpr::Reference(instantiate_affine_ref(r, offset)),
        AffineExpr::Name(s) => BoundExpr::Name(s.clone()),
        AffineExpr::Paren(inner) => {
            BoundExpr::Paren(Box::new(instantiate_affine_expr(inner, offset)))
        }
        AffineExpr::Unary { op, expr: inner } => BoundExpr::Unary {
            op: *op,
            expr: Box::new(instantiate_affine_expr(inner, offset)),
        },
        AffineExpr::Binary { op, left, right } => BoundExpr::Binary {
            op: *op,
            left: Box::new(instantiate_affine_expr(left, offset)),
            right: Box::new(instantiate_affine_expr(right, offset)),
        },
        AffineExpr::Call { name, args } => BoundExpr::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| instantiate_affine_expr(a, offset))
                .collect(),
        },
        AffineExpr::ScopedName {
            sheet,
            missing_sheet,
            name,
        } => BoundExpr::ScopedName {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            name: name.clone(),
        },
        AffineExpr::SpillRef(inner) => {
            BoundExpr::SpillRef(Box::new(instantiate_affine_expr(inner, offset)))
        }
        AffineExpr::MissingArg => BoundExpr::MissingArg,
    }
}

fn shift_affine_ref(r: &AffineRef, offset: u32) -> AffineRef {
    match r {
        AffineRef::Cell {
            sheet,
            missing_sheet,
            row,
            col,
            abs_col,
            abs_row,
        } => AffineRef::Cell {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            row: row.shifted(offset),
            col: *col,
            abs_col: *abs_col,
            abs_row: *abs_row,
        },
        AffineRef::Area {
            sheet,
            missing_sheet,
            first_row,
            first_col,
            last_row,
            last_col,
            abs_first_col,
            abs_first_row,
            abs_last_col,
            abs_last_row,
        } => AffineRef::Area {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            first_row: first_row.shifted(offset),
            first_col: *first_col,
            last_row: last_row.shifted(offset),
            last_col: *last_col,
            abs_first_col: *abs_first_col,
            abs_first_row: *abs_first_row,
            abs_last_col: *abs_last_col,
            abs_last_row: *abs_last_row,
        },
        AffineRef::ColRange { .. } | AffineRef::Invalid { .. } => r.clone(),
        AffineRef::RowRange {
            sheet,
            missing_sheet,
            first_row,
            last_row,
            abs_first,
            abs_last,
        } => AffineRef::RowRange {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            first_row: first_row.shifted(offset),
            last_row: last_row.shifted(offset),
            abs_first: *abs_first,
            abs_last: *abs_last,
        },
    }
}

fn shift_affine_expr(expr: &AffineExpr, offset: u32) -> AffineExpr {
    match expr {
        AffineExpr::Literal(_)
        | AffineExpr::ArrayLiteral { .. }
        | AffineExpr::Name(_)
        | AffineExpr::ScopedName { .. }
        | AffineExpr::MissingArg => expr.clone(),
        AffineExpr::Reference(r) => AffineExpr::Reference(shift_affine_ref(r, offset)),
        AffineExpr::Paren(inner) => AffineExpr::Paren(Box::new(shift_affine_expr(inner, offset))),
        AffineExpr::Unary { op, expr: inner } => AffineExpr::Unary {
            op: *op,
            expr: Box::new(shift_affine_expr(inner, offset)),
        },
        AffineExpr::Binary { op, left, right } => AffineExpr::Binary {
            op: *op,
            left: Box::new(shift_affine_expr(left, offset)),
            right: Box::new(shift_affine_expr(right, offset)),
        },
        AffineExpr::Call { name, args } => AffineExpr::Call {
            name: name.clone(),
            args: args.iter().map(|a| shift_affine_expr(a, offset)).collect(),
        },
        AffineExpr::SpillRef(inner) => {
            AffineExpr::SpillRef(Box::new(shift_affine_expr(inner, offset)))
        }
    }
}

fn affine_ref_can_merge(left: &AffineRef, left_len: u32, right: &AffineRef) -> bool {
    match (left, right) {
        (
            AffineRef::Cell {
                sheet: s0,
                missing_sheet: ms0,
                row: row0,
                col: col0,
                abs_col: ac0,
                abs_row: ar0,
            },
            AffineRef::Cell {
                sheet: s1,
                missing_sheet: ms1,
                row: row1,
                col: col1,
                abs_col: ac1,
                abs_row: ar1,
            },
        ) => {
            s0 == s1
                && ms0 == ms1
                && col0 == col1
                && ac0 == ac1
                && ar0 == ar1
                && row0.can_merge(left_len, *row1)
        }
        (
            AffineRef::Area {
                sheet: s0,
                missing_sheet: ms0,
                first_row: fr0,
                first_col: fc0,
                last_row: lr0,
                last_col: lc0,
                abs_first_col: afc0,
                abs_first_row: afr0,
                abs_last_col: alc0,
                abs_last_row: alr0,
            },
            AffineRef::Area {
                sheet: s1,
                missing_sheet: ms1,
                first_row: fr1,
                first_col: fc1,
                last_row: lr1,
                last_col: lc1,
                abs_first_col: afc1,
                abs_first_row: afr1,
                abs_last_col: alc1,
                abs_last_row: alr1,
            },
        ) => {
            s0 == s1
                && ms0 == ms1
                && fc0 == fc1
                && lc0 == lc1
                && afc0 == afc1
                && afr0 == afr1
                && alc0 == alc1
                && alr0 == alr1
                && fr0.can_merge(left_len, *fr1)
                && lr0.can_merge(left_len, *lr1)
        }
        (AffineRef::ColRange { .. }, AffineRef::ColRange { .. })
        | (AffineRef::Invalid { .. }, AffineRef::Invalid { .. }) => left == right,
        (
            AffineRef::RowRange {
                sheet: s0,
                missing_sheet: ms0,
                first_row: fr0,
                last_row: lr0,
                abs_first: af0,
                abs_last: al0,
            },
            AffineRef::RowRange {
                sheet: s1,
                missing_sheet: ms1,
                first_row: fr1,
                last_row: lr1,
                abs_first: af1,
                abs_last: al1,
            },
        ) => {
            s0 == s1
                && ms0 == ms1
                && af0 == af1
                && al0 == al1
                && fr0.can_merge(left_len, *fr1)
                && lr0.can_merge(left_len, *lr1)
        }
        _ => false,
    }
}

fn affine_expr_can_merge(left: &AffineExpr, left_len: u32, right: &AffineExpr) -> bool {
    match (left, right) {
        (AffineExpr::Literal(x), AffineExpr::Literal(y)) => lit_bits_eq(x, y),
        (
            AffineExpr::ArrayLiteral {
                rows: r0,
                cols: c0,
                elements: el0,
            },
            AffineExpr::ArrayLiteral {
                rows: r1,
                cols: c1,
                elements: el1,
            },
        ) => r0 == r1 && c0 == c1 && lits_bits_eq(el0, el1),
        (AffineExpr::Reference(a), AffineExpr::Reference(b)) => {
            affine_ref_can_merge(a, left_len, b)
        }
        (AffineExpr::Name(a), AffineExpr::Name(b)) => a == b,
        (AffineExpr::Paren(a), AffineExpr::Paren(b))
        | (AffineExpr::SpillRef(a), AffineExpr::SpillRef(b)) => {
            affine_expr_can_merge(a, left_len, b)
        }
        (AffineExpr::Unary { op: op0, expr: a }, AffineExpr::Unary { op: op1, expr: b }) => {
            op0 == op1 && affine_expr_can_merge(a, left_len, b)
        }
        (
            AffineExpr::Binary {
                op: op0,
                left: l0,
                right: r0,
            },
            AffineExpr::Binary {
                op: op1,
                left: l1,
                right: r1,
            },
        ) => {
            op0 == op1
                && affine_expr_can_merge(l0, left_len, l1)
                && affine_expr_can_merge(r0, left_len, r1)
        }
        (AffineExpr::Call { name: n0, args: a0 }, AffineExpr::Call { name: n1, args: a1 }) => {
            n0 == n1
                && a0.len() == a1.len()
                && a0
                    .iter()
                    .zip(a1)
                    .all(|(x, y)| affine_expr_can_merge(x, left_len, y))
        }
        (
            AffineExpr::ScopedName {
                sheet: s0,
                missing_sheet: ms0,
                name: n0,
            },
            AffineExpr::ScopedName {
                sheet: s1,
                missing_sheet: ms1,
                name: n1,
            },
        ) => s0 == s1 && ms0 == ms1 && n0 == n1,
        (AffineExpr::MissingArg, AffineExpr::MissingArg) => true,
        _ => false,
    }
}

fn validate_affine_expr(expr: &AffineExpr, depth: usize, len: u32) -> Result<(), OpError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(OpError::InvalidFormula);
    }
    match expr {
        AffineExpr::Literal(Literal::Number(n)) => {
            if !n.is_finite() {
                return Err(OpError::NonFiniteNumber);
            }
            Ok(())
        }
        AffineExpr::Literal(_)
        | AffineExpr::Name(_)
        | AffineExpr::ScopedName { .. }
        | AffineExpr::MissingArg => Ok(()),
        AffineExpr::Reference(r) => match r {
            AffineRef::Cell { row, .. } => row.validate(len),
            AffineRef::Area {
                first_row,
                last_row,
                ..
            }
            | AffineRef::RowRange {
                first_row,
                last_row,
                ..
            } => {
                first_row.validate(len)?;
                last_row.validate(len)
            }
            AffineRef::ColRange { .. } | AffineRef::Invalid { .. } => Ok(()),
        },
        AffineExpr::ArrayLiteral {
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
        AffineExpr::Paren(inner)
        | AffineExpr::Unary { expr: inner, .. }
        | AffineExpr::SpillRef(inner) => validate_affine_expr(inner, depth.saturating_add(1), len),
        AffineExpr::Binary { left, right, .. } => {
            validate_affine_expr(left, depth.saturating_add(1), len)?;
            validate_affine_expr(right, depth.saturating_add(1), len)
        }
        AffineExpr::Call { args, .. } => {
            if args.len() > MAX_FUNCTION_ARGS {
                return Err(OpError::InvalidFormula);
            }
            let next_depth = depth.saturating_add(1);
            for arg in args {
                validate_affine_expr(arg, next_depth, len)?;
            }
            Ok(())
        }
    }
}

fn affine_expr_bytes(expr: &AffineExpr) -> usize {
    let base = size_of::<AffineExpr>();
    match expr {
        AffineExpr::Literal(Literal::Text(s)) | AffineExpr::Name(s) => {
            base.saturating_add(s.capacity())
        }
        AffineExpr::ScopedName {
            missing_sheet,
            name,
            ..
        } => base
            .saturating_add(name.capacity())
            .saturating_add(missing_sheet.as_ref().map_or(0, String::capacity)),
        AffineExpr::Literal(_) | AffineExpr::Reference(_) | AffineExpr::MissingArg => base,
        AffineExpr::ArrayLiteral { elements, .. } => {
            base.saturating_add(elements.capacity().saturating_mul(size_of::<Literal>()))
        }
        AffineExpr::Paren(inner)
        | AffineExpr::Unary { expr: inner, .. }
        | AffineExpr::SpillRef(inner) => base.saturating_add(affine_expr_bytes(inner)),
        AffineExpr::Binary { left, right, .. } => base
            .saturating_add(affine_expr_bytes(left))
            .saturating_add(affine_expr_bytes(right)),
        AffineExpr::Call { name, args } => args
            .iter()
            .fold(base.saturating_add(name.capacity()), |acc, a| {
                acc.saturating_add(affine_expr_bytes(a))
            }),
    }
}

fn write_affine_expr_digest(expr: &AffineExpr, offset: u32, d: &mut Digest64) {
    match expr {
        AffineExpr::Literal(lit) => {
            d.write_bytes(&[1]);
            write_lit_digest(lit, d);
        }
        AffineExpr::ArrayLiteral {
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
        AffineExpr::Reference(r) => {
            d.write_bytes(&[3]);
            write_affine_ref_digest(r, offset, d);
        }
        AffineExpr::Name(s) => {
            d.write_bytes(&[4]);
            d.write_bytes(s.as_bytes());
        }
        AffineExpr::Paren(inner) => {
            d.write_bytes(&[5]);
            write_affine_expr_digest(inner, offset, d);
        }
        AffineExpr::Unary { op, expr: inner } => {
            let tag = match op {
                UnaryOp::Plus => 1_u8,
                UnaryOp::Neg => 2_u8,
                UnaryOp::ImplicitIntersection => 3_u8,
                UnaryOp::Percent => 4_u8,
            };
            d.write_bytes(&[6, tag]);
            write_affine_expr_digest(inner, offset, d);
        }
        AffineExpr::Binary { op, left, right } => {
            d.write_bytes(&[7]);
            d.write_bytes(op.as_str().as_bytes());
            write_affine_expr_digest(left, offset, d);
            write_affine_expr_digest(right, offset, d);
        }
        AffineExpr::Call { name, args } => {
            d.write_bytes(&[8]);
            d.write_bytes(name.as_bytes());
            d.write_u32(u32::try_from(args.len()).unwrap_or(u32::MAX));
            for arg in args {
                write_affine_expr_digest(arg, offset, d);
            }
        }
        AffineExpr::SpillRef(inner) => {
            d.write_bytes(&[9]);
            write_affine_expr_digest(inner, offset, d);
        }
        AffineExpr::ScopedName {
            sheet,
            missing_sheet,
            name,
        } => {
            d.write_bytes(&[10]);
            match sheet {
                None => d.write_bytes(&[0]),
                Some(sid) => {
                    d.write_bytes(&[1]);
                    d.write_u32(sid.0);
                }
            }
            match missing_sheet.as_deref() {
                None => d.write_bytes(&[0]),
                Some(ms) => {
                    d.write_bytes(&[1]);
                    d.write_u32(u32::try_from(ms.len()).unwrap_or(u32::MAX));
                    d.write_bytes(ms.as_bytes());
                }
            }
            d.write_u32(u32::try_from(name.len()).unwrap_or(u32::MAX));
            d.write_bytes(name.as_bytes());
        }
        AffineExpr::MissingArg => d.write_bytes(&[13]),
    }
}

fn write_affine_ref_digest(r: &AffineRef, offset: u32, d: &mut Digest64) {
    match r {
        AffineRef::Cell {
            sheet,
            row,
            col,
            abs_col,
            abs_row,
            ..
        } => {
            d.write_bytes(&[1, u8::from(*abs_col), u8::from(*abs_row)]);
            d.write_u32(sheet.map_or(u32::MAX, |s| s.0));
            d.write_u64(row.resolve(offset).0);
            d.write_u64(col.0);
        }
        AffineRef::Area {
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
            d.write_u64(first_row.resolve(offset).0);
            d.write_u64(first_col.0);
            d.write_u64(last_row.resolve(offset).0);
            d.write_u64(last_col.0);
        }
        AffineRef::ColRange {
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
        AffineRef::RowRange {
            sheet,
            first_row,
            last_row,
            abs_first,
            abs_last,
            ..
        } => {
            d.write_bytes(&[4, u8::from(*abs_first), u8::from(*abs_last)]);
            d.write_u32(sheet.map_or(u32::MAX, |s| s.0));
            d.write_u64(first_row.resolve(offset).0);
            d.write_u64(last_row.resolve(offset).0);
        }
        AffineRef::Invalid { sheet_name } => {
            d.write_bytes(&[5]);
            if let Some(s) = sheet_name {
                d.write_bytes(s.as_bytes());
            }
        }
    }
}

fn collect_affine_row_anchor(
    anchor: AffineRow,
    ref_sheet: Option<SheetId>,
    strip: &TemplateStrip<AffineFormula>,
    target_sheet: SheetId,
    ids: &[IdRun],
    out: &mut BTreeSet<WorkbookCellKey>,
) -> bool {
    if ref_sheet.unwrap_or(strip.sheet) != target_sheet {
        return false;
    }
    if anchor.stride == 0 {
        if id_in_runs(anchor.base, ids) {
            for offset in 0..strip.len {
                let row = RowId(strip.start_row.0.saturating_add(u64::from(offset)));
                out.insert((strip.sheet, row, strip.col));
            }
            return true;
        }
        return false;
    }
    let a_stream = id_stream(anchor.base);
    let a_end = anchor.base.saturating_add(u64::from(strip.len));
    for run in ids {
        if id_stream(run.first_id) != a_stream {
            continue;
        }
        let r_end = run.first_id.saturating_add(u64::from(run.len));
        let lo = anchor.base.max(run.first_id);
        let hi = a_end.min(r_end);
        for raw_r in lo..hi {
            let off = raw_r.saturating_sub(anchor.base);
            let row = RowId(strip.start_row.0.saturating_add(off));
            out.insert((strip.sheet, row, strip.col));
        }
    }
    false
}

fn collect_affine_row_endpoints(
    expr: &AffineExpr,
    strip: &TemplateStrip<AffineFormula>,
    target_sheet: SheetId,
    ids: &[IdRun],
    out: &mut BTreeSet<WorkbookCellKey>,
) -> bool {
    match expr {
        AffineExpr::Literal(_)
        | AffineExpr::ArrayLiteral { .. }
        | AffineExpr::Name(_)
        | AffineExpr::ScopedName { .. }
        | AffineExpr::MissingArg => false,
        AffineExpr::Reference(r) => match r {
            AffineRef::Area {
                sheet,
                first_row,
                last_row,
                ..
            }
            | AffineRef::RowRange {
                sheet,
                first_row,
                last_row,
                ..
            } => {
                if collect_affine_row_anchor(*first_row, *sheet, strip, target_sheet, ids, out) {
                    return true;
                }
                if last_row != first_row {
                    return collect_affine_row_anchor(
                        *last_row,
                        *sheet,
                        strip,
                        target_sheet,
                        ids,
                        out,
                    );
                }
                false
            }
            AffineRef::Cell { .. } | AffineRef::ColRange { .. } | AffineRef::Invalid { .. } => {
                false
            }
        },
        AffineExpr::Paren(inner)
        | AffineExpr::Unary { expr: inner, .. }
        | AffineExpr::SpillRef(inner) => {
            collect_affine_row_endpoints(inner, strip, target_sheet, ids, out)
        }
        AffineExpr::Binary { left, right, .. } => {
            collect_affine_row_endpoints(left, strip, target_sheet, ids, out)
                || collect_affine_row_endpoints(right, strip, target_sheet, ids, out)
        }
        AffineExpr::Call { args, .. } => {
            for arg in args {
                if collect_affine_row_endpoints(arg, strip, target_sheet, ids, out) {
                    return true;
                }
            }
            false
        }
    }
}

fn affine_expr_touches_col_endpoints(
    expr: &AffineExpr,
    strip_sheet: SheetId,
    target_sheet: SheetId,
    ids: &[IdRun],
) -> bool {
    match expr {
        AffineExpr::Literal(_)
        | AffineExpr::ArrayLiteral { .. }
        | AffineExpr::Name(_)
        | AffineExpr::ScopedName { .. }
        | AffineExpr::MissingArg => false,
        AffineExpr::Reference(r) => match r {
            AffineRef::Area {
                sheet,
                first_col,
                last_col,
                ..
            }
            | AffineRef::ColRange {
                sheet,
                first_col,
                last_col,
                ..
            } => {
                sheet.unwrap_or(strip_sheet) == target_sheet
                    && (id_in_runs(first_col.0, ids) || id_in_runs(last_col.0, ids))
            }
            AffineRef::Cell { .. } | AffineRef::RowRange { .. } | AffineRef::Invalid { .. } => {
                false
            }
        },
        AffineExpr::Paren(inner)
        | AffineExpr::Unary { expr: inner, .. }
        | AffineExpr::SpillRef(inner) => {
            affine_expr_touches_col_endpoints(inner, strip_sheet, target_sheet, ids)
        }
        AffineExpr::Binary { left, right, .. } => {
            affine_expr_touches_col_endpoints(left, strip_sheet, target_sheet, ids)
                || affine_expr_touches_col_endpoints(right, strip_sheet, target_sheet, ids)
        }
        AffineExpr::Call { args, .. } => args
            .iter()
            .any(|a| affine_expr_touches_col_endpoints(a, strip_sheet, target_sheet, ids)),
    }
}

fn validate_template_expr(expr: &TemplateExpr, depth: usize) -> Result<(), OpError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(OpError::InvalidFormula);
    }
    match expr {
        TemplateExpr::Literal(Literal::Number(n)) => {
            if !n.is_finite() {
                return Err(OpError::NonFiniteNumber);
            }
            Ok(())
        }
        TemplateExpr::Literal(_)
        | TemplateExpr::Reference(_)
        | TemplateExpr::Name(_)
        | TemplateExpr::ScopedName { .. }
        | TemplateExpr::MissingArg => Ok(()),
        TemplateExpr::Opaque { raw } => {
            if raw.chars().count() > MAX_FORMULA_CHARS {
                Err(OpError::FormulaTooLong)
            } else {
                Ok(())
            }
        }
        TemplateExpr::PreservedCached { raw, cached } => {
            if raw.chars().count() > MAX_FORMULA_CHARS {
                return Err(OpError::FormulaTooLong);
            }
            match cached {
                ScalarValue::Number(n) if !n.is_finite() => Err(OpError::NonFiniteNumber),
                ScalarValue::Text(s) if s.encode_utf16().count() > MAX_CELL_TEXT_UTF16 => {
                    Err(OpError::TextTooLong)
                }
                _ => Ok(()),
            }
        }
        TemplateExpr::ArrayLiteral {
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
        TemplateExpr::Paren(inner)
        | TemplateExpr::Unary { expr: inner, .. }
        | TemplateExpr::SpillRef(inner) => validate_template_expr(inner, depth.saturating_add(1)),
        TemplateExpr::Binary { left, right, .. } => {
            validate_template_expr(left, depth.saturating_add(1))?;
            validate_template_expr(right, depth.saturating_add(1))
        }
        TemplateExpr::Call { args, .. } => {
            if args.len() > MAX_FUNCTION_ARGS {
                return Err(OpError::InvalidFormula);
            }
            let next_depth = depth.saturating_add(1);
            for arg in args {
                validate_template_expr(arg, next_depth)?;
            }
            Ok(())
        }
    }
}

fn template_expanded_bytes(expr: &TemplateExpr) -> usize {
    let base = size_of::<BoundExpr>();
    match expr {
        TemplateExpr::Literal(Literal::Text(s))
        | TemplateExpr::Name(s)
        | TemplateExpr::Opaque { raw: s } => base.saturating_add(s.len()),
        TemplateExpr::ScopedName {
            missing_sheet,
            name,
            ..
        } => base
            .saturating_add(name.len())
            .saturating_add(missing_sheet.as_ref().map_or(0, String::len)),
        TemplateExpr::PreservedCached { raw, cached } => {
            let text_len = match cached {
                ScalarValue::Text(s) => s.len(),
                _ => 0,
            };
            base.saturating_add(raw.len()).saturating_add(text_len)
        }
        TemplateExpr::Literal(_) | TemplateExpr::Reference(_) | TemplateExpr::MissingArg => base,
        TemplateExpr::ArrayLiteral { elements, .. } => {
            base.saturating_add(elements.len().saturating_mul(size_of::<Literal>()))
        }
        TemplateExpr::Paren(inner)
        | TemplateExpr::Unary { expr: inner, .. }
        | TemplateExpr::SpillRef(inner) => base.saturating_add(template_expanded_bytes(inner)),
        TemplateExpr::Binary { left, right, .. } => base
            .saturating_add(template_expanded_bytes(left))
            .saturating_add(template_expanded_bytes(right)),
        TemplateExpr::Call { name, args } => {
            args.iter().fold(base.saturating_add(name.len()), |acc, a| {
                acc.saturating_add(template_expanded_bytes(a))
            })
        }
    }
}

fn expand_template_expr(expr: &TemplateExpr, self_row: RowId, self_col: ColId) -> BoundExpr {
    match expr {
        TemplateExpr::Literal(lit) => BoundExpr::Literal(lit.clone()),
        TemplateExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => BoundExpr::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.clone(),
        },
        TemplateExpr::Reference(tref) => {
            BoundExpr::Reference(expand_template_ref(tref, self_row, self_col))
        }
        TemplateExpr::Name(s) => BoundExpr::Name(s.clone()),
        TemplateExpr::ScopedName {
            sheet,
            missing_sheet,
            name,
        } => BoundExpr::ScopedName {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            name: name.clone(),
        },
        TemplateExpr::SpillRef(inner) => {
            BoundExpr::SpillRef(Box::new(expand_template_expr(inner, self_row, self_col)))
        }
        TemplateExpr::Opaque { raw } => BoundExpr::Opaque { raw: raw.clone() },
        TemplateExpr::PreservedCached { raw, cached } => BoundExpr::PreservedCached {
            raw: raw.clone(),
            cached: cached.clone(),
        },
        TemplateExpr::MissingArg => BoundExpr::MissingArg,
        TemplateExpr::Paren(inner) => {
            BoundExpr::Paren(Box::new(expand_template_expr(inner, self_row, self_col)))
        }
        TemplateExpr::Unary { op, expr: inner } => BoundExpr::Unary {
            op: *op,
            expr: Box::new(expand_template_expr(inner, self_row, self_col)),
        },
        TemplateExpr::Binary { op, left, right } => BoundExpr::Binary {
            op: *op,
            left: Box::new(expand_template_expr(left, self_row, self_col)),
            right: Box::new(expand_template_expr(right, self_row, self_col)),
        },
        TemplateExpr::Call { name, args } => BoundExpr::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| expand_template_expr(a, self_row, self_col))
                .collect(),
        },
    }
}

fn expand_template_ref(tref: &TemplateRef, self_row: RowId, self_col: ColId) -> BoundRef {
    match tref {
        TemplateRef::Cell {
            sheet,
            missing_sheet,
            row,
            col,
            abs_col,
            abs_row,
        } => BoundRef::Cell {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            row: RowId(row.resolve(self_row.0)),
            col: ColId(col.resolve(self_col.0)),
            abs_col: *abs_col,
            abs_row: *abs_row,
        },
        TemplateRef::Area {
            sheet,
            missing_sheet,
            first_row,
            first_col,
            last_row,
            last_col,
            abs_first_col,
            abs_first_row,
            abs_last_col,
            abs_last_row,
        } => BoundRef::Area {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            first_row: RowId(first_row.resolve(self_row.0)),
            first_col: ColId(first_col.resolve(self_col.0)),
            last_row: RowId(last_row.resolve(self_row.0)),
            last_col: ColId(last_col.resolve(self_col.0)),
            abs_first_col: *abs_first_col,
            abs_first_row: *abs_first_row,
            abs_last_col: *abs_last_col,
            abs_last_row: *abs_last_row,
        },
        TemplateRef::ColRange {
            sheet,
            missing_sheet,
            first_col,
            last_col,
            abs_first,
            abs_last,
        } => BoundRef::ColRange {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            first_col: ColId(first_col.resolve(self_col.0)),
            last_col: ColId(last_col.resolve(self_col.0)),
            abs_first: *abs_first,
            abs_last: *abs_last,
        },
        TemplateRef::RowRange {
            sheet,
            missing_sheet,
            first_row,
            last_row,
            abs_first,
            abs_last,
        } => BoundRef::RowRange {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            first_row: RowId(first_row.resolve(self_row.0)),
            last_row: RowId(last_row.resolve(self_row.0)),
            abs_first: *abs_first,
            abs_last: *abs_last,
        },
        TemplateRef::Invalid { sheet_name } => BoundRef::Invalid {
            sheet_name: sheet_name.clone(),
        },
    }
}

fn collect_expr_range_endpoints(
    expr: &BoundExpr,
    host_sheet: SheetId,
    row_eps: &mut Vec<(SheetId, RowId)>,
    col_eps: &mut Vec<(SheetId, ColId)>,
) {
    match expr {
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => {}
        BoundExpr::Reference(r) => match r {
            BoundRef::Area {
                sheet,
                first_row,
                first_col,
                last_row,
                last_col,
                ..
            } => {
                let target = sheet.unwrap_or(host_sheet);
                row_eps.push((target, *first_row));
                if *last_row != *first_row {
                    row_eps.push((target, *last_row));
                }
                col_eps.push((target, *first_col));
                if *last_col != *first_col {
                    col_eps.push((target, *last_col));
                }
            }
            BoundRef::RowRange {
                sheet,
                first_row,
                last_row,
                ..
            } => {
                let target = sheet.unwrap_or(host_sheet);
                row_eps.push((target, *first_row));
                if *last_row != *first_row {
                    row_eps.push((target, *last_row));
                }
            }
            BoundRef::ColRange {
                sheet,
                first_col,
                last_col,
                ..
            } => {
                let target = sheet.unwrap_or(host_sheet);
                col_eps.push((target, *first_col));
                if *last_col != *first_col {
                    col_eps.push((target, *last_col));
                }
            }
            BoundRef::Cell { .. } | BoundRef::Invalid { .. } => {}
        },
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => {
            collect_expr_range_endpoints(inner, host_sheet, row_eps, col_eps);
        }
        BoundExpr::Binary { left, right, .. } => {
            collect_expr_range_endpoints(left, host_sheet, row_eps, col_eps);
            collect_expr_range_endpoints(right, host_sheet, row_eps, col_eps);
        }
        BoundExpr::Call { args, .. } => {
            for arg in args {
                collect_expr_range_endpoints(arg, host_sheet, row_eps, col_eps);
            }
        }
    }
}

fn id_in_runs(id: u64, runs: &[IdRun]) -> bool {
    runs.iter().any(|r| r.contains(id))
}

/// Core E25 anchor handoff on a single axis (`14-m1b.md` 5.1 & S3b):
/// - If neither or both endpoints move, anchors stay unchanged;
/// - If only the high (`bottom`) endpoint moves: when `after_ord >= bottom_ord`
///   it stretches outward (unchanged); otherwise it hands off to the nearest
///   non-moved live entry in `top_ord..bottom_ord`;
/// - If only the low (`top`) endpoint moves: when `after == None` or
///   `after_ord < top_ord` it stretches outward (unchanged); otherwise it hands
///   off to the nearest non-moved live entry in `(top_ord + 1)..=bottom_ord`.
fn handoff_axis_pair(
    axis: &Axis,
    first: u64,
    last: u64,
    moved: &[IdRun],
    after: Option<u64>,
) -> Option<(u64, u64)> {
    let (Some(a), Some(b)) = (axis.axis_order_of(first), axis.axis_order_of(last)) else {
        return None;
    };
    let ((top, top_at), (bottom, bottom_at), swapped) = if a <= b {
        ((first, a), (last, b), false)
    } else {
        ((last, b), (first, a), true)
    };
    let top_moves = id_in_runs(top, moved);
    let bottom_moves = id_in_runs(bottom, moved);
    if top_moves == bottom_moves {
        return None;
    }

    let destination = after.and_then(|anchor| axis.axis_order_of(anchor));
    let mut new_top = top;
    let mut new_bottom = bottom;

    if bottom_moves {
        let outward = destination.is_some_and(|at| at >= bottom_at);
        if !outward {
            let mut chosen: Option<u64> = None;
            let mut fallback: Option<u64> = None;
            for ord in (top_at..bottom_at).rev() {
                if let Some((cand_id, is_live)) = axis.id_at_axis_order(ord)
                    && !id_in_runs(cand_id, moved)
                {
                    if fallback.is_none() {
                        fallback = Some(cand_id);
                    }
                    if is_live {
                        chosen = Some(cand_id);
                        break;
                    }
                }
            }
            if let Some(stay) = chosen.or(fallback) {
                new_bottom = stay;
            }
        }
    } else {
        let outward = destination.is_none_or(|at| at < top_at);
        if !outward {
            let mut chosen: Option<u64> = None;
            let mut fallback: Option<u64> = None;
            let start_ord = top_at.saturating_add(1);
            for ord in start_ord..=bottom_at {
                if let Some((cand_id, is_live)) = axis.id_at_axis_order(ord)
                    && !id_in_runs(cand_id, moved)
                {
                    if fallback.is_none() {
                        fallback = Some(cand_id);
                    }
                    if is_live {
                        chosen = Some(cand_id);
                        break;
                    }
                }
            }
            if let Some(stay) = chosen.or(fallback) {
                new_top = stay;
            }
        }
    }

    if (new_top, new_bottom) == (top, bottom) {
        None
    } else if swapped {
        Some((new_bottom, new_top))
    } else {
        Some((new_top, new_bottom))
    }
}

fn handoff_expr_rows(
    expr: &mut BoundExpr,
    host_sheet: SheetId,
    moved_sheet: SheetId,
    row_axis: &Axis,
    moved: &[IdRun],
    after: Option<u64>,
) -> bool {
    match expr {
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => false,
        BoundExpr::Reference(r) => match r {
            BoundRef::Area {
                sheet,
                first_row,
                last_row,
                ..
            }
            | BoundRef::RowRange {
                sheet,
                first_row,
                last_row,
                ..
            } => {
                if sheet.unwrap_or(host_sheet) != moved_sheet {
                    return false;
                }
                if let Some((nr1, nr2)) =
                    handoff_axis_pair(row_axis, first_row.0, last_row.0, moved, after)
                {
                    *first_row = RowId(nr1);
                    *last_row = RowId(nr2);
                    true
                } else {
                    false
                }
            }
            BoundRef::Cell { .. } | BoundRef::ColRange { .. } | BoundRef::Invalid { .. } => false,
        },
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => {
            handoff_expr_rows(inner, host_sheet, moved_sheet, row_axis, moved, after)
        }
        BoundExpr::Binary { left, right, .. } => {
            let l = handoff_expr_rows(left, host_sheet, moved_sheet, row_axis, moved, after);
            let r = handoff_expr_rows(right, host_sheet, moved_sheet, row_axis, moved, after);
            l || r
        }
        BoundExpr::Call { args, .. } => {
            let mut changed = false;
            for arg in args {
                if handoff_expr_rows(arg, host_sheet, moved_sheet, row_axis, moved, after) {
                    changed = true;
                }
            }
            changed
        }
    }
}

fn handoff_expr_cols(
    expr: &mut BoundExpr,
    host_sheet: SheetId,
    moved_sheet: SheetId,
    col_axis: &Axis,
    moved: &[IdRun],
    after: Option<u64>,
) -> bool {
    match expr {
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => false,
        BoundExpr::Reference(r) => match r {
            BoundRef::Area {
                sheet,
                first_col,
                last_col,
                ..
            }
            | BoundRef::ColRange {
                sheet,
                first_col,
                last_col,
                ..
            } => {
                if sheet.unwrap_or(host_sheet) != moved_sheet {
                    return false;
                }
                if let Some((nc1, nc2)) =
                    handoff_axis_pair(col_axis, first_col.0, last_col.0, moved, after)
                {
                    *first_col = ColId(nc1);
                    *last_col = ColId(nc2);
                    true
                } else {
                    false
                }
            }
            BoundRef::Cell { .. } | BoundRef::RowRange { .. } | BoundRef::Invalid { .. } => false,
        },
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => {
            handoff_expr_cols(inner, host_sheet, moved_sheet, col_axis, moved, after)
        }
        BoundExpr::Binary { left, right, .. } => {
            let l = handoff_expr_cols(left, host_sheet, moved_sheet, col_axis, moved, after);
            let r = handoff_expr_cols(right, host_sheet, moved_sheet, col_axis, moved, after);
            l || r
        }
        BoundExpr::Call { args, .. } => {
            let mut changed = false;
            for arg in args {
                if handoff_expr_cols(arg, host_sheet, moved_sheet, col_axis, moved, after) {
                    changed = true;
                }
            }
            changed
        }
    }
}

fn live_span_ids(axis: &Axis, first_anchor: u64, last_anchor: u64) -> Option<(u64, u64)> {
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
    Some((first_id?, last_id?))
}

fn finalize_expr_tombstones(expr: &mut BoundExpr, host_sheet: SheetId, sheets: &[Sheet]) -> bool {
    match expr {
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Name(_)
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => false,
        BoundExpr::ScopedName {
            sheet,
            missing_sheet,
            ..
        } => {
            if let Some(sid) = *sheet
                && !sheets.iter().any(|s| s.id() == sid && !s.is_deleted())
            {
                *sheet = None;
                *missing_sheet = None;
                return true;
            }
            false
        }
        BoundExpr::Reference(r) => finalize_ref_tombstones(r, host_sheet, sheets),
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => finalize_expr_tombstones(inner, host_sheet, sheets),
        BoundExpr::Binary { left, right, .. } => {
            let l = finalize_expr_tombstones(left, host_sheet, sheets);
            let r = finalize_expr_tombstones(right, host_sheet, sheets);
            l || r
        }
        BoundExpr::Call { args, .. } => {
            let mut changed = false;
            for arg in args {
                if finalize_expr_tombstones(arg, host_sheet, sheets) {
                    changed = true;
                }
            }
            changed
        }
    }
}

fn finalize_ref_tombstones(r: &mut BoundRef, host_sheet: SheetId, sheets: &[Sheet]) -> bool {
    let find_live_sheet =
        |id: SheetId| -> Option<&Sheet> { sheets.iter().find(|s| s.id() == id && !s.is_deleted()) };
    match r {
        BoundRef::Invalid { .. } => false,
        BoundRef::Cell {
            sheet, row, col, ..
        } => {
            let Some(s) = find_live_sheet(sheet.unwrap_or(host_sheet)) else {
                *r = BoundRef::Invalid { sheet_name: None };
                return true;
            };
            if !s.row_axis().is_live(row.0) || !s.col_axis().is_live(col.0) {
                *r = BoundRef::Invalid { sheet_name: None };
                return true;
            }
            false
        }
        BoundRef::Area {
            sheet,
            first_row,
            first_col,
            last_row,
            last_col,
            ..
        } => {
            let Some(s) = find_live_sheet(sheet.unwrap_or(host_sheet)) else {
                *r = BoundRef::Invalid { sheet_name: None };
                return true;
            };
            let (Some((r1, r2)), Some((c1, c2))) = (
                live_span_ids(s.row_axis(), first_row.0, last_row.0),
                live_span_ids(s.col_axis(), first_col.0, last_col.0),
            ) else {
                *r = BoundRef::Invalid { sheet_name: None };
                return true;
            };
            let mut changed = false;
            if first_row.0 != r1 {
                *first_row = RowId(r1);
                changed = true;
            }
            if last_row.0 != r2 {
                *last_row = RowId(r2);
                changed = true;
            }
            if first_col.0 != c1 {
                *first_col = ColId(c1);
                changed = true;
            }
            if last_col.0 != c2 {
                *last_col = ColId(c2);
                changed = true;
            }
            changed
        }
        BoundRef::ColRange {
            sheet,
            first_col,
            last_col,
            ..
        } => {
            let Some(s) = find_live_sheet(sheet.unwrap_or(host_sheet)) else {
                *r = BoundRef::Invalid { sheet_name: None };
                return true;
            };
            let Some((c1, c2)) = live_span_ids(s.col_axis(), first_col.0, last_col.0) else {
                *r = BoundRef::Invalid { sheet_name: None };
                return true;
            };
            let mut changed = false;
            if first_col.0 != c1 {
                *first_col = ColId(c1);
                changed = true;
            }
            if last_col.0 != c2 {
                *last_col = ColId(c2);
                changed = true;
            }
            changed
        }
        BoundRef::RowRange {
            sheet,
            first_row,
            last_row,
            ..
        } => {
            let Some(s) = find_live_sheet(sheet.unwrap_or(host_sheet)) else {
                *r = BoundRef::Invalid { sheet_name: None };
                return true;
            };
            let Some((r1, r2)) = live_span_ids(s.row_axis(), first_row.0, last_row.0) else {
                *r = BoundRef::Invalid { sheet_name: None };
                return true;
            };
            let mut changed = false;
            if first_row.0 != r1 {
                *first_row = RowId(r1);
                changed = true;
            }
            if last_row.0 != r2 {
                *last_row = RowId(r2);
                changed = true;
            }
            changed
        }
    }
}

/// Coordinate context for shifting a formula from `(src_sheet, src_row_pos, src_col_pos)`
/// to `(dst_sheet, dst_row_pos, dst_col_pos)` under E12.
#[derive(Clone, Copy, Debug)]
pub struct FormulaShiftSpec {
    /// Worksheet where the source formula lives.
    pub src_sheet: SheetId,
    /// 0-based visible row of the source cell.
    pub src_row_pos: u32,
    /// 0-based visible column of the source cell.
    pub src_col_pos: u32,
    /// Worksheet where the destination cell lives.
    pub dst_sheet: SheetId,
    /// 0-based visible row of the destination cell.
    pub dst_row_pos: u32,
    /// 0-based visible column of the destination cell.
    pub dst_col_pos: u32,
}

/// Shifts relative references in `formula` according to `spec` (semantic
/// checklist E12), turning references pushed off the sheet into `#REF!`.
#[must_use]
pub fn shift_formula_for_cell(
    formula: &BoundFormula,
    workbook: &Workbook<BoundFormula>,
    spec: FormulaShiftSpec,
) -> BoundFormula {
    let dr = i64::from(spec.dst_row_pos).saturating_sub(i64::from(spec.src_row_pos));
    let dc = i64::from(spec.dst_col_pos).saturating_sub(i64::from(spec.src_col_pos));
    let find_sheet = |id: SheetId| workbook.sheet(id);
    BoundFormula {
        mode: formula.mode,
        root: shift_bound_expr_with(
            &formula.root,
            &find_sheet,
            spec.src_sheet,
            spec.dst_sheet,
            dr,
            dc,
        ),
    }
}

fn shift_coord(pos: u32, delta: i64, is_abs: bool) -> Option<u32> {
    if is_abs {
        Some(pos)
    } else {
        let shifted = i64::from(pos).checked_add(delta)?;
        u32::try_from(shifted).ok()
    }
}

fn shift_bound_expr_with<'a>(
    expr: &BoundExpr,
    find_sheet: &dyn Fn(SheetId) -> Option<&'a Sheet>,
    src_sheet: SheetId,
    dst_sheet: SheetId,
    dr: i64,
    dc: i64,
) -> BoundExpr {
    match expr {
        BoundExpr::Literal(lit) => BoundExpr::Literal(lit.clone()),
        BoundExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => BoundExpr::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.clone(),
        },
        BoundExpr::Reference(r) => BoundExpr::Reference(shift_bound_ref_with(
            r, find_sheet, src_sheet, dst_sheet, dr, dc,
        )),
        BoundExpr::Name(s) => BoundExpr::Name(s.clone()),
        BoundExpr::ScopedName {
            sheet,
            missing_sheet,
            name,
        } => BoundExpr::ScopedName {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            name: name.clone(),
        },
        BoundExpr::SpillRef(inner) => BoundExpr::SpillRef(Box::new(shift_bound_expr_with(
            inner, find_sheet, src_sheet, dst_sheet, dr, dc,
        ))),
        BoundExpr::Opaque { raw } => BoundExpr::Opaque { raw: raw.clone() },
        BoundExpr::PreservedCached { raw, cached } => BoundExpr::PreservedCached {
            raw: raw.clone(),
            cached: cached.clone(),
        },
        BoundExpr::MissingArg => BoundExpr::MissingArg,
        BoundExpr::Paren(inner) => BoundExpr::Paren(Box::new(shift_bound_expr_with(
            inner, find_sheet, src_sheet, dst_sheet, dr, dc,
        ))),
        BoundExpr::Unary { op, expr: inner } => BoundExpr::Unary {
            op: *op,
            expr: Box::new(shift_bound_expr_with(
                inner, find_sheet, src_sheet, dst_sheet, dr, dc,
            )),
        },
        BoundExpr::Binary { op, left, right } => BoundExpr::Binary {
            op: *op,
            left: Box::new(shift_bound_expr_with(
                left, find_sheet, src_sheet, dst_sheet, dr, dc,
            )),
            right: Box::new(shift_bound_expr_with(
                right, find_sheet, src_sheet, dst_sheet, dr, dc,
            )),
        },
        BoundExpr::Call { name, args } => BoundExpr::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| shift_bound_expr_with(a, find_sheet, src_sheet, dst_sheet, dr, dc))
                .collect(),
        },
    }
}

fn shift_bound_ref_with<'a>(
    r: &BoundRef,
    find_sheet: &dyn Fn(SheetId) -> Option<&'a Sheet>,
    src_sheet: SheetId,
    dst_sheet: SheetId,
    dr: i64,
    dc: i64,
) -> BoundRef {
    match r {
        BoundRef::Invalid { sheet_name } => BoundRef::Invalid {
            sheet_name: sheet_name.clone(),
        },
        BoundRef::Cell {
            sheet,
            missing_sheet,
            row,
            col,
            abs_col,
            abs_row,
        } => {
            let (Some(ref_src), Some(ref_dst)) = (
                find_sheet(sheet.unwrap_or(src_sheet)),
                find_sheet(sheet.unwrap_or(dst_sheet)),
            ) else {
                return BoundRef::Invalid { sheet_name: None };
            };
            let (Some(orig_r), Some(orig_c)) = (
                ref_src.row_axis().id_to_pos(row.0),
                ref_src.col_axis().id_to_pos(col.0),
            ) else {
                return BoundRef::Invalid { sheet_name: None };
            };
            let (Some(new_r), Some(new_c)) = (
                shift_coord(orig_r, dr, *abs_row),
                shift_coord(orig_c, dc, *abs_col),
            ) else {
                return BoundRef::Invalid { sheet_name: None };
            };
            bind_cell_on_sheet(
                *sheet,
                missing_sheet.clone(),
                CellAddr {
                    col: new_c,
                    row: new_r,
                    abs_col: *abs_col,
                    abs_row: *abs_row,
                },
                ref_dst,
            )
        }
        BoundRef::Area {
            sheet,
            missing_sheet,
            first_row,
            first_col,
            last_row,
            last_col,
            abs_first_col,
            abs_first_row,
            abs_last_col,
            abs_last_row,
        } => {
            let (Some(ref_src), Some(ref_dst)) = (
                find_sheet(sheet.unwrap_or(src_sheet)),
                find_sheet(sheet.unwrap_or(dst_sheet)),
            ) else {
                return BoundRef::Invalid { sheet_name: None };
            };
            let (Some((r1, r2)), Some((c1, c2))) = (
                live_span_positions(ref_src.row_axis(), first_row.0, last_row.0),
                live_span_positions(ref_src.col_axis(), first_col.0, last_col.0),
            ) else {
                return BoundRef::Invalid { sheet_name: None };
            };
            let (Some(nr1), Some(nr2), Some(nc1), Some(nc2)) = (
                shift_coord(r1, dr, *abs_first_row),
                shift_coord(r2, dr, *abs_last_row),
                shift_coord(c1, dc, *abs_first_col),
                shift_coord(c2, dc, *abs_last_col),
            ) else {
                return BoundRef::Invalid { sheet_name: None };
            };
            bind_area_on_sheet(
                *sheet,
                missing_sheet.clone(),
                CellAddr {
                    col: nc1,
                    row: nr1,
                    abs_col: *abs_first_col,
                    abs_row: *abs_first_row,
                },
                CellAddr {
                    col: nc2,
                    row: nr2,
                    abs_col: *abs_last_col,
                    abs_row: *abs_last_row,
                },
                ref_dst,
            )
        }
        BoundRef::ColRange {
            sheet,
            missing_sheet,
            first_col,
            last_col,
            abs_first,
            abs_last,
        } => {
            let (Some(ref_src), Some(ref_dst)) = (
                find_sheet(sheet.unwrap_or(src_sheet)),
                find_sheet(sheet.unwrap_or(dst_sheet)),
            ) else {
                return BoundRef::Invalid { sheet_name: None };
            };
            let Some((c1, c2)) = live_span_positions(ref_src.col_axis(), first_col.0, last_col.0)
            else {
                return BoundRef::Invalid { sheet_name: None };
            };
            let (Some(nc1), Some(nc2)) = (
                shift_coord(c1, dc, *abs_first),
                shift_coord(c2, dc, *abs_last),
            ) else {
                return BoundRef::Invalid { sheet_name: None };
            };
            bind_cols_on_sheet(
                *sheet,
                missing_sheet.clone(),
                nc1,
                *abs_first,
                nc2,
                *abs_last,
                ref_dst,
            )
        }
        BoundRef::RowRange {
            sheet,
            missing_sheet,
            first_row,
            last_row,
            abs_first,
            abs_last,
        } => {
            let (Some(ref_src), Some(ref_dst)) = (
                find_sheet(sheet.unwrap_or(src_sheet)),
                find_sheet(sheet.unwrap_or(dst_sheet)),
            ) else {
                return BoundRef::Invalid { sheet_name: None };
            };
            let Some((r1, r2)) = live_span_positions(ref_src.row_axis(), first_row.0, last_row.0)
            else {
                return BoundRef::Invalid { sheet_name: None };
            };
            let (Some(nr1), Some(nr2)) = (
                shift_coord(r1, dr, *abs_first),
                shift_coord(r2, dr, *abs_last),
            ) else {
                return BoundRef::Invalid { sheet_name: None };
            };
            bind_rows_on_sheet(
                *sheet,
                missing_sheet.clone(),
                nr1,
                *abs_first,
                nr2,
                *abs_last,
                ref_dst,
            )
        }
    }
}

struct MoveCellsPlan<'a> {
    ctx: &'a MoveCellsRewriteCtx<'a>,
    src_sheet_ref: &'a Sheet,
    dst_sheet_ref: &'a Sheet,
    sr0: u32,
    sr1: u32,
    sc0: u32,
    sc1: u32,
    dr0: u32,
    dr1: u32,
    dc0: u32,
    dc1: u32,
    dr: i64,
    dc: i64,
}

impl<'a> MoveCellsPlan<'a> {
    fn from_ctx(ctx: &'a MoveCellsRewriteCtx<'a>) -> Option<Self> {
        let src_sheet_ref = ctx.sheets.iter().find(|s| s.id() == ctx.src_sheet)?;
        let dst_sheet_ref = ctx.sheets.iter().find(|s| s.id() == ctx.dst_sheet)?;
        let sr0 = src_sheet_ref
            .row_axis()
            .id_to_pos(ctx.src_rows.first()?.0)?;
        let sr1 = src_sheet_ref.row_axis().id_to_pos(ctx.src_rows.last()?.0)?;
        let sc0 = src_sheet_ref
            .col_axis()
            .id_to_pos(ctx.src_cols.first()?.0)?;
        let sc1 = src_sheet_ref.col_axis().id_to_pos(ctx.src_cols.last()?.0)?;
        let dr0 = dst_sheet_ref
            .row_axis()
            .id_to_pos(ctx.dst_rows.first()?.0)?;
        let dr1 = dst_sheet_ref.row_axis().id_to_pos(ctx.dst_rows.last()?.0)?;
        let dc0 = dst_sheet_ref
            .col_axis()
            .id_to_pos(ctx.dst_cols.first()?.0)?;
        let dc1 = dst_sheet_ref.col_axis().id_to_pos(ctx.dst_cols.last()?.0)?;
        Some(Self {
            ctx,
            src_sheet_ref,
            dst_sheet_ref,
            sr0,
            sr1,
            sc0,
            sc1,
            dr0,
            dr1,
            dc0,
            dc1,
            dr: i64::from(dr0).saturating_sub(i64::from(sr0)),
            dc: i64::from(dc0).saturating_sub(i64::from(sc0)),
        })
    }

    fn find_sheet(&self, id: SheetId) -> Option<&'a Sheet> {
        self.ctx.sheets.iter().find(|s| s.id() == id)
    }
}

fn rewrite_bound_expr_for_move_cells(
    expr: &mut BoundExpr,
    host_sheet_before: SheetId,
    is_moved_formula: bool,
    plan: &MoveCellsPlan<'_>,
) -> bool {
    match expr {
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => false,
        BoundExpr::Reference(r) => {
            rewrite_bound_ref_for_move_cells(r, host_sheet_before, is_moved_formula, plan)
        }
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => {
            rewrite_bound_expr_for_move_cells(inner, host_sheet_before, is_moved_formula, plan)
        }
        BoundExpr::Binary { left, right, .. } => {
            let l =
                rewrite_bound_expr_for_move_cells(left, host_sheet_before, is_moved_formula, plan);
            let r =
                rewrite_bound_expr_for_move_cells(right, host_sheet_before, is_moved_formula, plan);
            l || r
        }
        BoundExpr::Call { args, .. } => {
            let mut changed = false;
            for arg in args {
                if rewrite_bound_expr_for_move_cells(arg, host_sheet_before, is_moved_formula, plan)
                {
                    changed = true;
                }
            }
            changed
        }
    }
}

fn qualify_moved_cross_sheet(
    sheet: &mut Option<SheetId>,
    is_moved_formula: bool,
    plan: &MoveCellsPlan<'_>,
) -> bool {
    if is_moved_formula && plan.ctx.src_sheet != plan.ctx.dst_sheet && sheet.is_none() {
        *sheet = Some(plan.ctx.src_sheet);
        true
    } else {
        false
    }
}

fn rewrite_bound_ref_for_move_cells(
    r: &mut BoundRef,
    host_sheet_before: SheetId,
    is_moved_formula: bool,
    plan: &MoveCellsPlan<'_>,
) -> bool {
    let ctx = plan.ctx;
    let host_after = if is_moved_formula {
        ctx.dst_sheet
    } else {
        host_sheet_before
    };
    match r {
        BoundRef::Invalid { .. } => false,
        BoundRef::ColRange { sheet, .. } | BoundRef::RowRange { sheet, .. } => {
            qualify_moved_cross_sheet(sheet, is_moved_formula, plan)
        }
        BoundRef::Cell {
            sheet,
            missing_sheet,
            row,
            col,
            ..
        } => {
            let target_sheet = sheet.unwrap_or(host_sheet_before);
            if target_sheet == ctx.src_sheet
                && let (Some(rp), Some(cp)) = (
                    plan.src_sheet_ref.row_axis().id_to_pos(row.0),
                    plan.src_sheet_ref.col_axis().id_to_pos(col.0),
                )
                && (plan.sr0..=plan.sr1).contains(&rp)
                && (plan.sc0..=plan.sc1).contains(&cp)
            {
                let ri = usize::try_from(rp.saturating_sub(plan.sr0)).unwrap_or(usize::MAX);
                let ci = usize::try_from(cp.saturating_sub(plan.sc0)).unwrap_or(usize::MAX);
                if let (Some(&new_r), Some(&new_c)) = (ctx.dst_rows.get(ri), ctx.dst_cols.get(ci)) {
                    *row = new_r;
                    *col = new_c;
                    *missing_sheet = None;
                    *sheet = if sheet.is_some() || ctx.dst_sheet != host_after {
                        Some(ctx.dst_sheet)
                    } else {
                        None
                    };
                    return true;
                }
                return false;
            }
            if target_sheet == ctx.dst_sheet
                && let (Some(rp), Some(cp)) = (
                    plan.dst_sheet_ref.row_axis().id_to_pos(row.0),
                    plan.dst_sheet_ref.col_axis().id_to_pos(col.0),
                )
                && (plan.dr0..=plan.dr1).contains(&rp)
                && (plan.dc0..=plan.dc1).contains(&cp)
            {
                *r = BoundRef::Invalid { sheet_name: None };
                return true;
            }
            qualify_moved_cross_sheet(sheet, is_moved_formula, plan)
        }
        BoundRef::Area {
            sheet,
            missing_sheet,
            first_row,
            first_col,
            last_row,
            last_col,
            ..
        } => {
            let target_sheet = sheet.unwrap_or(host_sheet_before);
            let Some(target) = plan.find_sheet(target_sheet) else {
                return qualify_moved_cross_sheet(sheet, is_moved_formula, plan);
            };
            let (Some(p_fr), Some(p_lr), Some(p_fc), Some(p_lc)) = (
                target.row_axis().id_to_pos(first_row.0),
                target.row_axis().id_to_pos(last_row.0),
                target.col_axis().id_to_pos(first_col.0),
                target.col_axis().id_to_pos(last_col.0),
            ) else {
                return qualify_moved_cross_sheet(sheet, is_moved_formula, plan);
            };
            let (low_r, high_r, ar0, ar1) = if p_fr <= p_lr {
                (first_row, last_row, p_fr, p_lr)
            } else {
                (last_row, first_row, p_lr, p_fr)
            };
            let (low_c, high_c, ac0, ac1) = if p_fc <= p_lc {
                (first_col, last_col, p_fc, p_lc)
            } else {
                (last_col, first_col, p_lc, p_fc)
            };

            // 1. Entire area fits within `src`: move all 4 anchors into `dst`.
            if target_sheet == ctx.src_sheet
                && ar0 >= plan.sr0
                && ar1 <= plan.sr1
                && ac0 >= plan.sc0
                && ac1 <= plan.sc1
                && let (Some(&nr0), Some(&nr1), Some(&nc0), Some(&nc1)) = (
                    ctx.dst_rows
                        .get(usize::try_from(ar0.saturating_sub(plan.sr0)).unwrap_or(usize::MAX)),
                    ctx.dst_rows
                        .get(usize::try_from(ar1.saturating_sub(plan.sr0)).unwrap_or(usize::MAX)),
                    ctx.dst_cols
                        .get(usize::try_from(ac0.saturating_sub(plan.sc0)).unwrap_or(usize::MAX)),
                    ctx.dst_cols
                        .get(usize::try_from(ac1.saturating_sub(plan.sc0)).unwrap_or(usize::MAX)),
                )
            {
                *low_r = nr0;
                *high_r = nr1;
                *low_c = nc0;
                *high_c = nc1;
                *missing_sheet = None;
                *sheet = if sheet.is_some() || ctx.dst_sheet != host_after {
                    Some(ctx.dst_sheet)
                } else {
                    None
                };
                return true;
            }

            // 2. Same-sheet corner / full-edge outward stretches matching Excel 16.0.
            if target_sheet == ctx.src_sheet && ctx.src_sheet == ctx.dst_sheet {
                if plan.dc == 0 && plan.sc0 <= ac0 && ac1 <= plan.sc1 && ar0 < ar1 {
                    if plan.sr0 > ar0
                        && plan.sr0 <= ar1
                        && ar1 <= plan.sr1
                        && plan.dr > 0
                        && let Some(&nr) = ctx.dst_rows.get(
                            usize::try_from(ar1.saturating_sub(plan.sr0)).unwrap_or(usize::MAX),
                        )
                    {
                        *high_r = nr;
                        return true;
                    }
                    if plan.sr0 <= ar0
                        && ar0 <= plan.sr1
                        && plan.sr1 < ar1
                        && plan.dr < 0
                        && let Some(&nr) = ctx.dst_rows.get(
                            usize::try_from(ar0.saturating_sub(plan.sr0)).unwrap_or(usize::MAX),
                        )
                    {
                        *low_r = nr;
                        return true;
                    }
                }
                if plan.dr == 0 && plan.sr0 <= ar0 && ar1 <= plan.sr1 && ac0 < ac1 {
                    if plan.sc0 > ac0
                        && plan.sc0 <= ac1
                        && ac1 <= plan.sc1
                        && plan.dc > 0
                        && let Some(&nc) = ctx.dst_cols.get(
                            usize::try_from(ac1.saturating_sub(plan.sc0)).unwrap_or(usize::MAX),
                        )
                    {
                        *high_c = nc;
                        return true;
                    }
                    if plan.sc0 <= ac0
                        && ac0 <= plan.sc1
                        && plan.sc1 < ac1
                        && plan.dc < 0
                        && let Some(&nc) = ctx.dst_cols.get(
                            usize::try_from(ac0.saturating_sub(plan.sc0)).unwrap_or(usize::MAX),
                        )
                    {
                        *low_c = nc;
                        return true;
                    }
                }
                if plan.sr0 == plan.sr1 && plan.sc0 == plan.sc1 && (ar0 < ar1 || ac0 < ac1) {
                    if plan.sr0 == ar1
                        && plan.sc0 == ac1
                        && plan.dr >= 0
                        && plan.dc >= 0
                        && (plan.dr > 0 || plan.dc > 0)
                        && let (Some(&nr), Some(&nc)) = (ctx.dst_rows.first(), ctx.dst_cols.first())
                    {
                        *high_r = nr;
                        *high_c = nc;
                        return true;
                    }
                    if plan.sr0 == ar0
                        && plan.sc0 == ac0
                        && plan.dr <= 0
                        && plan.dc <= 0
                        && (plan.dr < 0 || plan.dc < 0)
                        && let (Some(&nr), Some(&nc)) = (ctx.dst_rows.first(), ctx.dst_cols.first())
                    {
                        *low_r = nr;
                        *low_c = nc;
                        return true;
                    }
                }
            }

            // 3. Otherwise leave endpoints unchanged (qualifying sheet if formula moved cross-sheet).
            qualify_moved_cross_sheet(sheet, is_moved_formula, plan)
        }
    }
}

/// Converts a bound formula at `(self_row, self_col)` into a relative-ID
/// [`TemplateFormula`].
#[must_use]
pub fn templatize_formula(
    bound: &BoundFormula,
    self_row: RowId,
    self_col: ColId,
) -> TemplateFormula {
    TemplateFormula {
        mode: bound.mode,
        root: templatize_expr(&bound.root, self_row, self_col),
    }
}

fn templatize_expr(expr: &BoundExpr, self_row: RowId, self_col: ColId) -> TemplateExpr {
    match expr {
        BoundExpr::Literal(lit) => TemplateExpr::Literal(lit.clone()),
        BoundExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => TemplateExpr::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.clone(),
        },
        BoundExpr::Reference(r) => TemplateExpr::Reference(templatize_ref(r, self_row, self_col)),
        BoundExpr::Name(s) => TemplateExpr::Name(s.clone()),
        BoundExpr::ScopedName {
            sheet,
            missing_sheet,
            name,
        } => TemplateExpr::ScopedName {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            name: name.clone(),
        },
        BoundExpr::SpillRef(inner) => {
            TemplateExpr::SpillRef(Box::new(templatize_expr(inner, self_row, self_col)))
        }
        BoundExpr::Opaque { raw } => TemplateExpr::Opaque { raw: raw.clone() },
        BoundExpr::PreservedCached { raw, cached } => TemplateExpr::PreservedCached {
            raw: raw.clone(),
            cached: cached.clone(),
        },
        BoundExpr::MissingArg => TemplateExpr::MissingArg,
        BoundExpr::Paren(inner) => {
            TemplateExpr::Paren(Box::new(templatize_expr(inner, self_row, self_col)))
        }
        BoundExpr::Unary { op, expr: inner } => TemplateExpr::Unary {
            op: *op,
            expr: Box::new(templatize_expr(inner, self_row, self_col)),
        },
        BoundExpr::Binary { op, left, right } => TemplateExpr::Binary {
            op: *op,
            left: Box::new(templatize_expr(left, self_row, self_col)),
            right: Box::new(templatize_expr(right, self_row, self_col)),
        },
        BoundExpr::Call { name, args } => TemplateExpr::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| templatize_expr(a, self_row, self_col))
                .collect(),
        },
    }
}

fn templatize_ref(r: &BoundRef, self_row: RowId, self_col: ColId) -> TemplateRef {
    match r {
        BoundRef::Cell {
            sheet,
            missing_sheet,
            row,
            col,
            abs_col,
            abs_row,
        } => TemplateRef::Cell {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            row: AxisTarget::from_bound(row.0, self_row.0, *abs_row),
            col: AxisTarget::from_bound(col.0, self_col.0, *abs_col),
            abs_col: *abs_col,
            abs_row: *abs_row,
        },
        BoundRef::Area {
            sheet,
            missing_sheet,
            first_row,
            first_col,
            last_row,
            last_col,
            abs_first_col,
            abs_first_row,
            abs_last_col,
            abs_last_row,
        } => TemplateRef::Area {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            first_row: AxisTarget::from_bound(first_row.0, self_row.0, *abs_first_row),
            first_col: AxisTarget::from_bound(first_col.0, self_col.0, *abs_first_col),
            last_row: AxisTarget::from_bound(last_row.0, self_row.0, *abs_last_row),
            last_col: AxisTarget::from_bound(last_col.0, self_col.0, *abs_last_col),
            abs_first_col: *abs_first_col,
            abs_first_row: *abs_first_row,
            abs_last_col: *abs_last_col,
            abs_last_row: *abs_last_row,
        },
        BoundRef::ColRange {
            sheet,
            missing_sheet,
            first_col,
            last_col,
            abs_first,
            abs_last,
        } => TemplateRef::ColRange {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            first_col: AxisTarget::from_bound(first_col.0, self_col.0, *abs_first),
            last_col: AxisTarget::from_bound(last_col.0, self_col.0, *abs_last),
            abs_first: *abs_first,
            abs_last: *abs_last,
        },
        BoundRef::RowRange {
            sheet,
            missing_sheet,
            first_row,
            last_row,
            abs_first,
            abs_last,
        } => TemplateRef::RowRange {
            sheet: *sheet,
            missing_sheet: missing_sheet.clone(),
            first_row: AxisTarget::from_bound(first_row.0, self_row.0, *abs_first),
            last_row: AxisTarget::from_bound(last_row.0, self_row.0, *abs_last),
            abs_first: *abs_first,
            abs_last: *abs_last,
        },
        BoundRef::Invalid { sheet_name } => TemplateRef::Invalid {
            sheet_name: sheet_name.clone(),
        },
    }
}

/// Target rectangle for [`build_fill_plan`].
#[derive(Clone, Copy, Debug)]
pub struct FillTargetRect {
    /// Destination worksheet.
    pub dst_sheet: SheetId,
    /// First 0-based visible row of the target rectangle.
    pub first_row_pos: u32,
    /// Number of visible rows to fill.
    pub row_count: u32,
    /// First 0-based visible column of the target rectangle.
    pub first_col_pos: u32,
    /// Number of visible columns to fill.
    pub col_count: u32,
}

/// Builds a compact [`Op::FillFormula`] (+ seam [`Op::SetCells`] fallback when
/// crossing axis run discontinuities) operation list in `O(runs * refs)` time
/// whose expanded formulas are bit-for-bit identical to shifting `source_formula`
/// into every cell of `rect` individually (`14-m1b.md` 5.2).
#[must_use]
pub fn build_fill_plan(
    source_formula: &BoundFormula,
    workbook: &Workbook<BoundFormula>,
    src_sheet: SheetId,
    src_row_pos: u32,
    src_col_pos: u32,
    rect: FillTargetRect,
) -> Vec<Op<BoundFormula>> {
    if rect.row_count == 0 || rect.col_count == 0 {
        return Vec::new();
    }
    let Some(dst) = workbook.sheet(rect.dst_sheet) else {
        return Vec::new();
    };
    let end_row = rect
        .first_row_pos
        .saturating_add(rect.row_count)
        .min(dst.rows());
    let end_col = rect
        .first_col_pos
        .saturating_add(rect.col_count)
        .min(dst.cols());
    if rect.first_row_pos >= end_row || rect.first_col_pos >= end_col {
        return Vec::new();
    }

    let row_splits = compute_axis_splits(
        &source_formula.root,
        workbook,
        src_sheet,
        rect.dst_sheet,
        src_row_pos,
        rect.first_row_pos,
        end_row,
        true,
    );
    let col_splits = compute_axis_splits(
        &source_formula.root,
        workbook,
        src_sheet,
        rect.dst_sheet,
        src_col_pos,
        rect.first_col_pos,
        end_col,
        false,
    );

    let row_segments = build_contiguous_segments(dst.row_axis(), &row_splits);
    let col_segments = build_contiguous_segments(dst.col_axis(), &col_splits);

    // Group sub-blocks: merge consecutive row segments when `col_segments.len() == 1`
    // (or across matching column runs) that share an identical `TemplateFormula`.
    let mut template_groups: Vec<(TemplateFormula, Vec<IdRun>, Vec<IdRun>, usize)> = Vec::new();
    let mut seam_edits: Vec<CellEdit<BoundFormula>> = Vec::new();

    for &(c_pos, c_run) in &col_segments {
        for &(r_pos, r_run) in &row_segments {
            let rep_bound = shift_formula_for_cell(
                source_formula,
                workbook,
                FormulaShiftSpec {
                    src_sheet,
                    src_row_pos,
                    src_col_pos,
                    dst_sheet: rect.dst_sheet,
                    dst_row_pos: r_pos,
                    dst_col_pos: c_pos,
                },
            );
            let seg_cells = (r_run.len as usize).saturating_mul(c_run.len as usize);
            let tpl = templatize_formula(&rep_bound, RowId(r_run.first_id), ColId(c_run.first_id));

            // Check if an existing group has the exact same template and matching `cols`.
            let c_vec = vec![c_run];
            if let Some(existing) = template_groups
                .iter_mut()
                .find(|(t, _, cs, _)| *t == tpl && *cs == c_vec)
            {
                existing.1.push(r_run);
                existing.3 = existing.3.saturating_add(seg_cells);
            } else {
                template_groups.push((tpl, vec![r_run], c_vec, seg_cells));
            }
        }
    }

    // Coalesce groups that share the same `(TemplateFormula, rows)` across columns too.
    let mut merged_groups: Vec<(TemplateFormula, Vec<IdRun>, Vec<IdRun>, usize)> = Vec::new();
    for (tpl, r_runs, c_runs, count) in template_groups {
        if let Some(existing) = merged_groups
            .iter_mut()
            .find(|(t, rs, _, _)| *t == tpl && *rs == r_runs)
        {
            existing.2.extend(c_runs);
            existing.3 = existing.3.saturating_add(count);
        } else {
            merged_groups.push((tpl, r_runs, c_runs, count));
        }
    }

    let has_bulk_group = merged_groups.iter().any(|(_, _, _, cnt)| *cnt > 1);
    let mut ops = Vec::new();
    for (tpl, r_runs, c_runs, count) in merged_groups {
        // Per `14-m1b.md` 5.2 step 3: when there is a main `FillFormula` bulk
        // group and a single-cell seam boundary exception, emit seam cells in a
        // companion `Op::SetCells`; otherwise emit `Op::FillFormula`.
        if has_bulk_group && count == 1 && seam_edits.len() < MAX_EDITS_PER_OP {
            if let (Some(r_run), Some(c_run)) = (r_runs.first(), c_runs.first()) {
                let row = RowId(r_run.first_id);
                let col = ColId(c_run.first_id);
                let expanded = BoundFormula::expand_template(&tpl, row, col);
                seam_edits.push(CellEdit {
                    row,
                    col,
                    input: CellInput::Formula {
                        formula: expanded,
                        cached: None,
                    },
                    format: FormatPatch::Keep,
                });
            }
        } else {
            ops.push(Op::FillFormula {
                sheet: rect.dst_sheet,
                rows: r_runs,
                cols: c_runs,
                template: tpl,
            });
        }
    }
    if !seam_edits.is_empty() {
        ops.push(Op::SetCells {
            sheet: rect.dst_sheet,
            edits: seam_edits,
        });
    }
    ops
}

fn build_contiguous_segments(axis: &Axis, splits: &BTreeSet<u32>) -> Vec<(u32, IdRun)> {
    let points: Vec<u32> = splits.iter().copied().collect();
    let mut out = Vec::new();
    for pair in points.windows(2) {
        let &[start, end] = pair else {
            continue;
        };
        let count = end.saturating_sub(start);
        if count == 0 {
            continue;
        }
        let mut cur_pos = start;
        axis.for_each_live_positions(start, count, |run| {
            out.push((cur_pos, run));
            cur_pos = cur_pos.saturating_add(run.len);
        });
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn compute_axis_splits(
    expr: &BoundExpr,
    workbook: &Workbook<BoundFormula>,
    src_sheet: SheetId,
    dst_sheet: SheetId,
    src_pos: u32,
    first_pos: u32,
    end_pos: u32,
    is_row_axis: bool,
) -> BTreeSet<u32> {
    let mut splits = BTreeSet::new();
    splits.insert(first_pos);
    splits.insert(end_pos);

    if let Some(dst) = workbook.sheet(dst_sheet) {
        let dst_axis = if is_row_axis {
            dst.row_axis()
        } else {
            dst.col_axis()
        };
        let mut cur = first_pos;
        dst_axis.for_each_live_positions(first_pos, end_pos.saturating_sub(first_pos), |run| {
            if cur > first_pos && cur < end_pos {
                splits.insert(cur);
            }
            cur = cur.saturating_add(run.len);
        });
    }

    collect_ref_splits(
        expr,
        workbook,
        src_sheet,
        dst_sheet,
        src_pos,
        first_pos,
        end_pos,
        is_row_axis,
        &mut splits,
    );
    splits
}

#[allow(clippy::too_many_arguments)]
fn collect_ref_splits(
    expr: &BoundExpr,
    workbook: &Workbook<BoundFormula>,
    src_sheet: SheetId,
    dst_sheet: SheetId,
    src_pos: u32,
    first_pos: u32,
    end_pos: u32,
    is_row_axis: bool,
    splits: &mut BTreeSet<u32>,
) {
    match expr {
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => {}
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => collect_ref_splits(
            inner,
            workbook,
            src_sheet,
            dst_sheet,
            src_pos,
            first_pos,
            end_pos,
            is_row_axis,
            splits,
        ),
        BoundExpr::Binary { left, right, .. } => {
            collect_ref_splits(
                left,
                workbook,
                src_sheet,
                dst_sheet,
                src_pos,
                first_pos,
                end_pos,
                is_row_axis,
                splits,
            );
            collect_ref_splits(
                right,
                workbook,
                src_sheet,
                dst_sheet,
                src_pos,
                first_pos,
                end_pos,
                is_row_axis,
                splits,
            );
        }
        BoundExpr::Call { args, .. } => {
            for arg in args {
                collect_ref_splits(
                    arg,
                    workbook,
                    src_sheet,
                    dst_sheet,
                    src_pos,
                    first_pos,
                    end_pos,
                    is_row_axis,
                    splits,
                );
            }
        }
        BoundExpr::Reference(r) => {
            let add_split = |cand: i64, set: &mut BTreeSet<u32>| {
                if cand > i64::from(first_pos)
                    && cand < i64::from(end_pos)
                    && let Ok(u) = u32::try_from(cand)
                {
                    set.insert(u);
                }
            };
            let add_rel_coord_splits = |orig_pos: u32, ref_axis: &Axis, set: &mut BTreeSet<u32>| {
                let offset = i64::from(orig_pos).saturating_sub(i64::from(src_pos));
                let live_len = i64::from(ref_axis.live_len());
                add_split(0_i64.saturating_sub(offset), set);
                add_split(live_len.saturating_sub(offset), set);

                let q_min_i = i64::from(first_pos)
                    .saturating_add(offset)
                    .clamp(0, live_len);
                let q_max_i = i64::from(end_pos).saturating_add(offset).clamp(0, live_len);
                if let (Ok(q_min), Ok(q_max)) = (u32::try_from(q_min_i), u32::try_from(q_max_i))
                    && q_min < q_max
                {
                    let mut cur_q = q_min;
                    ref_axis.for_each_live_positions(q_min, q_max.saturating_sub(q_min), |run| {
                        add_split(i64::from(cur_q).saturating_sub(offset), set);
                        cur_q = cur_q.saturating_add(run.len);
                    });
                }
            };
            let add_mixed_cross_splits =
                |rel_orig: u32, fixed_orig: u32, set: &mut BTreeSet<u32>| {
                    let offset = i64::from(rel_orig).saturating_sub(i64::from(src_pos));
                    let cross = i64::from(fixed_orig).saturating_sub(offset);
                    add_split(cross, set);
                    add_split(cross.saturating_add(1), set);
                };

            match r {
                BoundRef::Invalid { .. } => {}
                BoundRef::Cell {
                    sheet,
                    row,
                    col,
                    abs_col,
                    abs_row,
                    ..
                } => {
                    let (Some(ref_src), Some(ref_dst)) = (
                        workbook.sheet(sheet.unwrap_or(src_sheet)),
                        workbook.sheet(sheet.unwrap_or(dst_sheet)),
                    ) else {
                        return;
                    };
                    if is_row_axis {
                        if !*abs_row && let Some(orig_r) = ref_src.row_axis().id_to_pos(row.0) {
                            add_rel_coord_splits(orig_r, ref_dst.row_axis(), splits);
                        }
                    } else if !*abs_col && let Some(orig_c) = ref_src.col_axis().id_to_pos(col.0) {
                        add_rel_coord_splits(orig_c, ref_dst.col_axis(), splits);
                    }
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
                    let (Some(ref_src), Some(ref_dst)) = (
                        workbook.sheet(sheet.unwrap_or(src_sheet)),
                        workbook.sheet(sheet.unwrap_or(dst_sheet)),
                    ) else {
                        return;
                    };
                    if is_row_axis {
                        if let Some((r1, r2)) =
                            live_span_positions(ref_src.row_axis(), first_row.0, last_row.0)
                        {
                            if !*abs_first_row {
                                add_rel_coord_splits(r1, ref_dst.row_axis(), splits);
                            }
                            if !*abs_last_row {
                                add_rel_coord_splits(r2, ref_dst.row_axis(), splits);
                            }
                            if !*abs_first_row && *abs_last_row {
                                add_mixed_cross_splits(r1, r2, splits);
                            } else if *abs_first_row && !*abs_last_row {
                                add_mixed_cross_splits(r2, r1, splits);
                            }
                        }
                    } else if let Some((c1, c2)) =
                        live_span_positions(ref_src.col_axis(), first_col.0, last_col.0)
                    {
                        if !*abs_first_col {
                            add_rel_coord_splits(c1, ref_dst.col_axis(), splits);
                        }
                        if !*abs_last_col {
                            add_rel_coord_splits(c2, ref_dst.col_axis(), splits);
                        }
                        if !*abs_first_col && *abs_last_col {
                            add_mixed_cross_splits(c1, c2, splits);
                        } else if *abs_first_col && !*abs_last_col {
                            add_mixed_cross_splits(c2, c1, splits);
                        }
                    }
                }
                BoundRef::ColRange {
                    sheet,
                    first_col,
                    last_col,
                    abs_first,
                    abs_last,
                    ..
                } => {
                    if is_row_axis {
                        return;
                    }
                    let (Some(ref_src), Some(ref_dst)) = (
                        workbook.sheet(sheet.unwrap_or(src_sheet)),
                        workbook.sheet(sheet.unwrap_or(dst_sheet)),
                    ) else {
                        return;
                    };
                    if let Some((c1, c2)) =
                        live_span_positions(ref_src.col_axis(), first_col.0, last_col.0)
                    {
                        if !*abs_first {
                            add_rel_coord_splits(c1, ref_dst.col_axis(), splits);
                        }
                        if !*abs_last {
                            add_rel_coord_splits(c2, ref_dst.col_axis(), splits);
                        }
                        if !*abs_first && *abs_last {
                            add_mixed_cross_splits(c1, c2, splits);
                        } else if *abs_first && !*abs_last {
                            add_mixed_cross_splits(c2, c1, splits);
                        }
                    }
                }
                BoundRef::RowRange {
                    sheet,
                    first_row,
                    last_row,
                    abs_first,
                    abs_last,
                    ..
                } => {
                    if !is_row_axis {
                        return;
                    }
                    let (Some(ref_src), Some(ref_dst)) = (
                        workbook.sheet(sheet.unwrap_or(src_sheet)),
                        workbook.sheet(sheet.unwrap_or(dst_sheet)),
                    ) else {
                        return;
                    };
                    if let Some((r1, r2)) =
                        live_span_positions(ref_src.row_axis(), first_row.0, last_row.0)
                    {
                        if !*abs_first {
                            add_rel_coord_splits(r1, ref_dst.row_axis(), splits);
                        }
                        if !*abs_last {
                            add_rel_coord_splits(r2, ref_dst.row_axis(), splits);
                        }
                        if !*abs_first && *abs_last {
                            add_mixed_cross_splits(r1, r2, splits);
                        } else if *abs_first && !*abs_last {
                            add_mixed_cross_splits(r2, r1, splits);
                        }
                    }
                }
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

    use alloc::string::String;
    use alloc::vec;

    use reticula_core::id::{ColId, IdRun, RowId, SheetId};
    use reticula_core::ops::{FormulaMode, Op, Workbook};
    use reticula_core::value::{CalcError, Value};

    use super::{
        BoundFormula, FillTargetRect, FormulaShiftSpec, build_fill_plan, shift_formula_for_cell,
    };
    use crate::engine::FormulaEngine;

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

    /// Verifies all 20 S3b Excel 16.0.20711 row-move cases (`spikes/excel/results/row-moves-excel-16.0.20711.txt`)
    /// for E13 (single-cell follow) and E25 (range anchor handoff), plus exact `InverseOp` restoration.
    #[test]
    fn all_20_s3b_excel_row_moves_match_formula_text_and_sum_and_invert_cleanly() {
        struct RowMoveCase {
            m1: u64,
            m2: u64,
            above_row: u64,
            exp_sum_f: &'static str,
            exp_sum: f64,
            exp_c5: &'static str,
            exp_c7: &'static str,
            exp_c9: &'static str,
        }
        #[allow(clippy::too_many_arguments)]
        const fn rc(
            m1: u64,
            m2: u64,
            above_row: u64,
            exp_sum_f: &'static str,
            exp_sum: f64,
            exp_c5: &'static str,
            exp_c7: &'static str,
            exp_c9: &'static str,
        ) -> RowMoveCase {
            RowMoveCase {
                m1,
                m2,
                above_row,
                exp_sum_f,
                exp_sum,
                exp_c5,
                exp_c7,
                exp_c9,
            }
        }
        let cases: &[RowMoveCase] = &[
            // Interior row leaves or moves within the range
            rc(7, 7, 2, "=SUM(A6:A9)", 28.0, "=A6", "=A2", "=A9"),
            rc(7, 7, 12, "=SUM(A5:A8)", 28.0, "=A5", "=A11", "=A8"),
            rc(6, 6, 9, "=SUM(A5:A9)", 35.0, "=A5", "=A6", "=A9"),
            // Last row of the range moves
            rc(9, 9, 12, "=SUM(A5:A11)", 56.0, "=A5", "=A7", "=A11"),
            rc(9, 9, 7, "=SUM(A5:A9)", 35.0, "=A5", "=A8", "=A7"),
            rc(9, 9, 5, "=SUM(A6:A9)", 26.0, "=A6", "=A8", "=A5"),
            rc(9, 9, 2, "=SUM(A6:A9)", 26.0, "=A6", "=A8", "=A2"),
            // First row of the range moves
            rc(5, 5, 2, "=SUM(A2:A9)", 44.0, "=A2", "=A7", "=A9"),
            rc(5, 5, 8, "=SUM(A5:A9)", 35.0, "=A7", "=A6", "=A9"),
            rc(5, 5, 10, "=SUM(A5:A8)", 30.0, "=A9", "=A6", "=A8"),
            rc(5, 5, 12, "=SUM(A5:A8)", 30.0, "=A11", "=A6", "=A8"),
            // Outside row moves in
            rc(11, 11, 7, "=SUM(A5:A10)", 46.0, "=A5", "=A8", "=A10"),
            rc(11, 11, 5, "=SUM(A6:A10)", 35.0, "=A6", "=A8", "=A10"),
            rc(11, 11, 10, "=SUM(A5:A9)", 35.0, "=A5", "=A7", "=A9"),
            rc(2, 2, 7, "=SUM(A4:A9)", 37.0, "=A4", "=A7", "=A9"),
            rc(2, 2, 10, "=SUM(A4:A8)", 35.0, "=A4", "=A6", "=A8"),
            // Several rows at once
            rc(5, 6, 12, "=SUM(A5:A7)", 24.0, "=A10", "=A5", "=A7"),
            rc(8, 9, 2, "=SUM(A7:A9)", 18.0, "=A7", "=A9", "=A3"),
            rc(5, 9, 2, "=SUM(A2:A6)", 35.0, "=A2", "=A4", "=A6"),
            rc(4, 10, 2, "=SUM(A3:A7)", 35.0, "=A3", "=A5", "=A7"),
        ];

        for c in cases {
            let (m1, m2, above_row, exp_sum_f, exp_sum, exp_c5, exp_c7, exp_c9) = (
                c.m1,
                c.m2,
                c.above_row,
                c.exp_sum_f,
                c.exp_sum,
                c.exp_c5,
                c.exp_c7,
                c.exp_c9,
            );
            let mut eng = FormulaEngine::new();
            let sheet = SheetId(0);
            for r in 1..=12_u64 {
                eng.edit_cell_raw(sheet, RowId(r - 1), ColId(0), &alloc::format!("{r}"))
                    .unwrap();
            }
            eng.edit_cell_raw(sheet, RowId(0), ColId(1), "=SUM(A5:A9)")
                .unwrap();
            eng.edit_cell_raw(sheet, RowId(1), ColId(1), "=A5").unwrap();
            eng.edit_cell_raw(sheet, RowId(2), ColId(1), "=A7").unwrap();
            eng.edit_cell_raw(sheet, RowId(3), ColId(1), "=A9").unwrap();
            eng.recalc_all();
            let baseline_digest = eng.workbook().logical_digest();

            let len = u32::try_from(m2 - m1 + 1).unwrap();
            let ids = vec![IdRun::new(m1 - 1, len).unwrap()];
            let after = if above_row <= 1 {
                None
            } else {
                Some(RowId(above_row - 2))
            };
            let inv = eng
                .apply_op(Op::MoveRows { sheet, ids, after })
                .expect("move rows succeeds");
            eng.recalc_all();

            let sum_cell = eng.inspect_cell(sheet, RowId(0), ColId(1)).unwrap();
            let c5_cell = eng.inspect_cell(sheet, RowId(1), ColId(1)).unwrap();
            let c7_cell = eng.inspect_cell(sheet, RowId(2), ColId(1)).unwrap();
            let c9_cell = eng.inspect_cell(sheet, RowId(3), ColId(1)).unwrap();

            assert_eq!(sum_cell.raw, exp_sum_f, "case ({m1}..={m2} -> {above_row})");
            assert_eq!(
                eng.workbook()
                    .sheet(sheet)
                    .unwrap()
                    .value_by_id(RowId(0), ColId(1)),
                Value::Number(exp_sum),
                "sum mismatch on case ({m1}..={m2} -> {above_row})"
            );
            assert_eq!(c5_cell.raw, exp_c5);
            assert_eq!(c7_cell.raw, exp_c7);
            assert_eq!(c9_cell.raw, exp_c9);
            assert_eq!(
                eng.workbook()
                    .sheet(sheet)
                    .unwrap()
                    .value_by_id(RowId(1), ColId(1)),
                Value::Number(5.0)
            );
            assert_eq!(
                eng.workbook()
                    .sheet(sheet)
                    .unwrap()
                    .value_by_id(RowId(2), ColId(1)),
                Value::Number(7.0)
            );
            assert_eq!(
                eng.workbook()
                    .sheet(sheet)
                    .unwrap()
                    .value_by_id(RowId(3), ColId(1)),
                Value::Number(9.0)
            );

            // InverseOp restores exact logical digest and original formula bindings!
            eng.apply_inverse(inv).unwrap();
            eng.recalc_all();
            assert_eq!(eng.workbook().logical_digest(), baseline_digest);
        }
    }

    #[test]
    fn move_cols_handoff_and_cross_sheet_e20_e28_round_trip() {
        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        // Populate E1:I1 (`ColId(4..=8)`) with 5..=9 and put `=SUM(E1:I1)` in A2.
        for c in 4..=8_u64 {
            eng.edit_cell_raw(s0, RowId(0), ColId(c), &alloc::format!("{}", c + 1))
                .unwrap();
        }
        eng.edit_cell_raw(s0, RowId(1), ColId(0), "=SUM(E1:I1)")
            .unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(1), ColId(0)),
            Value::Number(35.0)
        );

        // Move column I (`ColId(8)`) inward before column G (`after = ColId(5)`): E25 hands off to H -> sum stays 35!
        let inv_col = eng
            .apply_op(Op::MoveCols {
                sheet: s0,
                ids: vec![IdRun::new(8, 1).unwrap()],
                after: Some(ColId(5)),
            })
            .unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.inspect_cell(s0, RowId(1), ColId(0)).unwrap().raw,
            "=SUM(E1:I1)"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(1), ColId(0)),
            Value::Number(35.0)
        );
        eng.apply_inverse(inv_col).unwrap();
        eng.recalc_all();

        // E28 duplicate sheet name disambiguation & E20 cross-sheet delete/undelete.
        let s1 = eng.alloc_sheet_id();
        eng.apply_op(Op::AddSheet {
            sheet: s1,
            name: String::from("Data"),
            after: Some(s0),
        })
        .unwrap();
        let s2 = eng.alloc_sheet_id();
        eng.apply_op(Op::AddSheet {
            sheet: s2,
            name: String::from("data"),
            after: Some(s1),
        })
        .unwrap();
        assert_eq!(eng.workbook().sheet(s2).unwrap().name(), "data (2)");

        eng.edit_cell_raw(s1, RowId(0), ColId(0), "42").unwrap();
        eng.edit_cell_raw(s0, RowId(2), ColId(0), "=Data!A1")
            .unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(2), ColId(0)),
            Value::Number(42.0)
        );

        // Rename `Data` -> `Sales 2026`: formula bar shows quoted sheet name without rewriting AST!
        eng.apply_op(Op::RenameSheet {
            sheet: s1,
            name: String::from("Sales 2026"),
        })
        .unwrap();
        assert_eq!(
            eng.inspect_cell(s0, RowId(2), ColId(0)).unwrap().raw,
            "='Sales 2026'!A1"
        );

        // Delete `s1` (E20): cross-sheet reference becomes `=#REF!A1` with value `#REF!`.
        eng.apply_op(Op::DeleteSheet { sheet: s1 }).unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.inspect_cell(s0, RowId(2), ColId(0)).unwrap().raw,
            "=#REF!A1"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(2), ColId(0)),
            Value::Error(CalcError::Ref)
        );
        // Re-parsing `=#REF!A1+1` also succeeds without syntax error.
        let reparsed =
            BoundFormula::parse_and_bind("=#REF!A1+1", FormulaMode::Dynamic, eng.workbook(), s0)
                .unwrap();
        assert_eq!(reparsed.unparse(eng.workbook(), s0), "=#REF!+1");

        // Undelete `s1` (`Ctrl+Z` forward op): `s0!A3` automatically recovers `42`!
        eng.apply_op(Op::UndeleteSheet { sheet: s1 }).unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.inspect_cell(s0, RowId(2), ColId(0)).unwrap().raw,
            "='Sales 2026'!A1"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(2), ColId(0)),
            Value::Number(42.0)
        );
    }

    #[test]
    fn fill_formula_encoded_byte_size_is_constant_from_10_rows_to_1_million_rows() {
        // Build a sheet with 1,000,000 live rows (matching default sample axis scale).
        let eng = FormulaEngine::open_sample(2026_1117, 1_000);
        let s0 = SheetId(0);
        let src_f = eng.workbook().formula(s0, RowId(1), ColId(12)).unwrap();

        let mut sheet_1m = reticula_core::sheet::Sheet::standard(s0, String::from("Sheet1"));
        *sheet_1m.row_axis_mut() =
            reticula_core::axis::Axis::with_base(1_000_000, reticula_core::axis::MAX_ROW_AXIS_LEN);
        *sheet_1m.col_axis_mut() =
            reticula_core::axis::Axis::with_base(26, reticula_core::axis::MAX_COL_AXIS_LEN);
        let wb_base_1m: Workbook<BoundFormula> = Workbook::from_sheet(sheet_1m, None);

        let ops_10 = build_fill_plan(
            src_f.as_ref(),
            &wb_base_1m,
            s0,
            1,
            12,
            FillTargetRect {
                dst_sheet: s0,
                first_row_pos: 2,
                row_count: 10,
                first_col_pos: 12,
                col_count: 1,
            },
        );
        let ops_1m = build_fill_plan(
            src_f.as_ref(),
            &wb_base_1m,
            s0,
            1,
            12,
            FillTargetRect {
                dst_sheet: s0,
                first_row_pos: 2,
                row_count: 999_998,
                first_col_pos: 12,
                col_count: 1,
            },
        );
        assert_eq!(ops_10.len(), 1);
        assert_eq!(ops_1m.len(), 1);

        let bytes_10 = postcard::to_allocvec(&ops_10).unwrap();
        let bytes_1m = postcard::to_allocvec(&ops_1m).unwrap();
        // Both are small constant-size (< 140 bytes); 1M rows adds at most 2 varint bytes for `len`!
        assert!(
            bytes_1m.len() <= 140,
            "1M fill encoded to {} B",
            bytes_1m.len()
        );
        assert!(bytes_1m.len().abs_diff(bytes_10.len()) <= 3);

        // Verify E12 copy shift and seam fallback across a tombstoned row.
        let mut eng_seam = FormulaEngine::new();
        eng_seam
            .edit_cell_raw(s0, RowId(1), ColId(1), "=A1+$A$2+A$3+$A4")
            .unwrap();
        eng_seam
            .apply_op(Op::DeleteRows {
                sheet: s0,
                ids: vec![IdRun::new(4, 1).unwrap()],
            })
            .unwrap();
        let seam_src = eng_seam
            .workbook()
            .formula(s0, RowId(1), ColId(1))
            .unwrap()
            .into_owned();
        let plan = build_fill_plan(
            &seam_src,
            eng_seam.workbook(),
            s0,
            1,
            1,
            FillTargetRect {
                dst_sheet: s0,
                first_row_pos: 2,
                row_count: 10,
                first_col_pos: 1,
                col_count: 1,
            },
        );
        eng_seam.apply_txn(plan).unwrap();
        for pos in 2..12_u32 {
            let rid = RowId(eng_seam.workbook().sheet(s0).unwrap().row_id(pos));
            let expected = shift_formula_for_cell(
                &seam_src,
                eng_seam.workbook(),
                FormulaShiftSpec {
                    src_sheet: s0,
                    src_row_pos: 1,
                    src_col_pos: 1,
                    dst_sheet: s0,
                    dst_row_pos: pos,
                    dst_col_pos: 1,
                },
            );
            let actual = eng_seam.workbook().formula(s0, rid, ColId(1)).unwrap();
            assert_eq!(actual.as_ref(), &expected, "mismatch at pos {pos}");
        }
    }

    #[test]
    fn local_tombstone_and_string_gc_preserves_live_semantics() {
        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        let s1 = eng.alloc_sheet_id();
        eng.apply_op(Op::AddSheet {
            sheet: s1,
            name: String::from("Temp"),
            after: Some(s0),
        })
        .unwrap();
        for r in 0..5_u64 {
            eng.edit_cell_raw(s0, RowId(r), ColId(0), &alloc::format!("{}", (r + 1) * 10))
                .unwrap();
        }
        eng.edit_cell_raw(s0, RowId(3), ColId(2), "orphan_before_overwrite")
            .unwrap();
        eng.edit_cell_raw(s0, RowId(3), ColId(2), "kept_text")
            .unwrap();
        eng.edit_cell_raw(s0, RowId(0), ColId(1), "=SUM(A1:A5)")
            .unwrap();
        eng.edit_cell_raw(s0, RowId(1), ColId(1), "=A1").unwrap();
        eng.edit_cell_raw(s0, RowId(2), ColId(1), "=Temp!A1")
            .unwrap();

        // Delete row 0 (top endpoint of A1:A5 and target of `=A1`) and sheet `Temp`.
        eng.apply_op(Op::DeleteRows {
            sheet: s0,
            ids: vec![IdRun::new(0, 1).unwrap()],
        })
        .unwrap();
        eng.apply_op(Op::DeleteSheet { sheet: s1 }).unwrap();
        eng.recalc_all();

        let sum_before = eng.inspect_cell(s0, RowId(1), ColId(1)); // Wait: RowId(0) was deleted!
        let _ = sum_before;
        // Put `=SUM(A1:A5)` in RowId(6) so the formula cell itself survives row 0 deletion!
        eng.edit_cell_raw(s0, RowId(6), ColId(1), "=SUM(A1:A4)")
            .unwrap();
        // Delete RowId(1) (current top of A1:A4):
        eng.apply_op(Op::DeleteRows {
            sheet: s0,
            ids: vec![IdRun::new(1, 1).unwrap()],
        })
        .unwrap();
        eng.recalc_all();

        let sum_text_before = eng.inspect_cell(s0, RowId(6), ColId(1)).unwrap().raw;
        let dead_cell_before = eng.inspect_cell(s0, RowId(2), ColId(1)).unwrap().display;
        assert_eq!(sum_text_before, "=SUM(A1:A3)");
        assert_eq!(dead_cell_before, "#REF!");

        eng.compact_local_garbage();

        assert_eq!(eng.workbook().sheets().len(), 1);
        assert_eq!(eng.workbook().sheet(s0).unwrap().strings().len(), 1);
        assert_eq!(
            eng.inspect_cell(s0, RowId(6), ColId(1)).unwrap().raw,
            "=SUM(A1:A3)"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(6), ColId(1)),
            Value::Number(120.0) // 30 + 40 + 50
        );
        assert_eq!(
            eng.inspect_cell(s0, RowId(2), ColId(1)).unwrap().raw,
            "=#REF!"
        );
    }

    #[test]
    fn remap_ids_rewrites_bound_and_template_references() {
        use alloc::collections::BTreeMap;
        use reticula_core::id::pack_id;

        use super::{BoundExpr, BoundRef};

        let mut wb = Workbook::new();
        let s0 = SheetId(0);
        let old_r = wb.allocator_mut().alloc_run(1).unwrap();
        let new_r = RowId(pack_id(1, 5, 1).unwrap());
        let new_s = SheetId(99);
        wb.apply(Op::InsertRows {
            sheet: s0,
            after: None,
            ids: old_r,
        })
        .unwrap();
        let mut bound =
            BoundFormula::parse_and_bind("=SUM(A1:A2)", FormulaMode::Dynamic, &wb, s0).unwrap();
        let mut rows = BTreeMap::new();
        rows.insert(RowId(old_r.first_id), new_r);
        let mut sheets = BTreeMap::new();
        sheets.insert(s0, new_s);
        bound.remap_ids(&sheets, &rows, &BTreeMap::new());
        if let BoundExpr::Call { args, .. } = &bound.root
            && let BoundExpr::Reference(BoundRef::Area { first_row, .. }) = &args[0]
        {
            assert_eq!(*first_row, new_r);
        } else {
            panic!("expected BoundRef::Area");
        }
    }

    #[test]
    fn strip_compression_roundtrips_fill_split_merge_and_digest_parity() {
        use reticula_core::ops::{CellEdit, CellInput, FormatPatch, ScalarValue};
        use reticula_core::sheet::Sheet;

        use super::templatize_formula;

        let mut sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
        *sheet.row_axis_mut() =
            reticula_core::axis::Axis::with_base(100_050, reticula_core::axis::MAX_ROW_AXIS_LEN);
        *sheet.col_axis_mut() =
            reticula_core::axis::Axis::with_base(10, reticula_core::axis::MAX_COL_AXIS_LEN);
        let mut wb = Workbook::<BoundFormula>::from_sheet(sheet, None);
        let s0 = SheetId(0);

        let f_sliding =
            BoundFormula::parse_and_bind("=A1+$B$1+SUM(C1:C5)", FormulaMode::Dynamic, &wb, s0)
                .unwrap();
        let f_lookup = BoundFormula::parse_and_bind(
            "=VLOOKUP(A1,$B$2:$C$100,2,FALSE)",
            FormulaMode::Dynamic,
            &wb,
            s0,
        )
        .unwrap();
        let t_sliding = templatize_formula(&f_sliding, RowId(0), ColId(3));
        let t_lookup = templatize_formula(&f_lookup, RowId(0), ColId(4));

        // Fill 100,000 rows in ColId(3) and 1,000 rows in ColId(4).
        let inv1 = wb
            .apply(Op::FillFormula {
                sheet: s0,
                rows: vec![IdRun::new(0, 100_000).unwrap()],
                cols: vec![IdRun::new(3, 1).unwrap()],
                template: t_sliding,
            })
            .unwrap();
        wb.apply(Op::FillFormula {
            sheet: s0,
            rows: vec![IdRun::new(0, 1_000).unwrap()],
            cols: vec![IdRun::new(4, 1).unwrap()],
            template: t_lookup,
        })
        .unwrap();

        assert_eq!(wb.formulas().len(), 101_000);
        assert_eq!(wb.formulas().strip_count(), 2);
        assert_eq!(wb.formulas().single_count(), 0);
        assert!(wb.formulas().estimated_bytes() < 4_096);
        assert_eq!(
            wb.content_digest(),
            wb.compute_content_digest_from_scratch()
        );

        // Overwrite a middle cell to split the 100k strip, then undo to re-coalesce it.
        let inv_edit = wb
            .apply(Op::SetCells {
                sheet: s0,
                edits: vec![CellEdit {
                    row: RowId(50_000),
                    col: ColId(3),
                    input: CellInput::Scalar(ScalarValue::Number(7.0)),
                    format: FormatPatch::Keep,
                }],
            })
            .unwrap();
        assert_eq!(wb.formulas().len(), 100_999);
        assert_eq!(wb.formulas().strip_count(), 3);
        assert_eq!(
            wb.content_digest(),
            wb.compute_content_digest_from_scratch()
        );

        wb.apply_inverse(inv_edit).unwrap();
        assert_eq!(wb.formulas().len(), 101_000);
        assert_eq!(wb.formulas().strip_count(), 2);
        assert_eq!(wb.formulas().single_count(), 0);
        assert_eq!(
            wb.content_digest(),
            wb.compute_content_digest_from_scratch()
        );

        // Check range endpoint handoff when moving an anchor row touched by fixed VLOOKUP & sliding SUM.
        let inv_move = wb
            .apply(Op::MoveRows {
                sheet: s0,
                ids: vec![IdRun::new(99, 1).unwrap()],
                after: Some(RowId(10)),
            })
            .unwrap();
        assert_eq!(
            wb.content_digest(),
            wb.compute_content_digest_from_scratch()
        );
        wb.apply_inverse(inv_move).unwrap();
        assert_eq!(
            wb.content_digest(),
            wb.compute_content_digest_from_scratch()
        );

        wb.apply_inverse(inv1).unwrap();
        assert_eq!(wb.formulas().len(), 1_000);
        assert_eq!(wb.formulas().strip_count(), 1);
    }
}
