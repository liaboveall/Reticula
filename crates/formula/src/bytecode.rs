//! Flat register-based bytecode compiler for eligible 1D vertical [`StripSlot`]
//! formulas (`19-m3b.md` §4).
//!
//! Compiles an [`AffineFormula`] into a chunked [`CompiledStripProgram`] over
//! virtual [`RegVec`](crate::batch_eval::RegVec) registers so `evaluate_strip_slot`
//! can process up to `1024` rows per dispatch with zero per-row AST traversal.
//! Unsupported constructs cleanly return `None` from [`compile_strip_program`]
//! so evaluation falls back to per-row AST interpretation.

use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::decimal15::RoundingMode;
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::num;
use reticula_core::ops::FormulaMode;
use reticula_core::value::CalcError;
use smallvec::SmallVec;

use crate::ast::{BinaryOp, Literal, MAX_NESTING_DEPTH, UnaryOp};
use crate::binder::{AffineExpr, AffineFormula, AffineRef};

/// Number of rows processed per strip batch evaluation chunk (`u32`).
pub(crate) const CHUNK_LEN_U32: u32 = 1024;

/// Number of rows processed per strip batch evaluation chunk.
pub(crate) const CHUNK_LEN: usize = CHUNK_LEN_U32 as usize;

/// Maximum virtual registers permitted in a compiled strip bytecode program.
const MAX_VIRTUAL_REGS: usize = 256;

/// Rectangular fixed worksheet area (`stride == 0` on both row anchors).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FixedAreaSpec {
    /// Target worksheet identity.
    pub sheet: SheetId,
    /// Top anchor row identity.
    pub first_row: RowId,
    /// Left anchor column identity.
    pub first_col: ColId,
    /// Bottom anchor row identity.
    pub last_row: RowId,
    /// Right anchor column identity.
    pub last_col: ColId,
}

/// Non-self sliding column read (`row.stride == 1`) within a strip formula.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StrideColSpec {
    /// Target worksheet identity.
    pub sheet: SheetId,
    /// Row identity read at strip offset `0`.
    pub base_row: u64,
    /// Column identity read across the chunk.
    pub col: ColId,
}

/// Unary numeric opcode supported by the strip bytecode interpreter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnaryByteOp {
    /// Unary `+x`.
    Plus,
    /// Unary `-x`.
    Neg,
    /// Postfix percentage `x%` (`x / 100`).
    Percent,
}

/// Flat register bytecode instruction over `1024`-row vector registers.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Instr {
    /// Broadcast finite numeric constant.
    ConstNum { dst: u16, val: f64 },
    /// Broadcast boolean constant.
    ConstBool { dst: u16, val: bool },
    /// Broadcast constant text string indexed into `CompiledStripProgram::const_strings`.
    ConstText { dst: u16, const_str_idx: usize },
    /// Broadcast error constant.
    ConstError { dst: u16, err: CalcError },
    /// Broadcast empty (`MissingArg`) marker inside `IF` branches.
    ConstEmpty { dst: u16 },
    /// Broadcast a fixed scalar cell `(sheet, row, col)`.
    LoadFixedCell {
        dst: u16,
        sheet: SheetId,
        row: RowId,
        col: ColId,
    },
    /// Load a sliding `stride == 1` cell column (either preloaded or recurrence back-reference).
    LoadStrideCell {
        dst: u16,
        /// Index into `CompiledStripProgram::stride_cols` when `self_back_offset` is `None`.
        stride_idx: usize,
        /// Non-`None` iff this references `self` strip's column at `offset - back`.
        self_back_offset: Option<u32>,
    },
    /// Zero-argument `ROW()` returning `base_live_pos + lane + 1` per row.
    RowZeroArgs { dst: u16 },
    /// Unary operator (`+`, `-`, `%`).
    Unary { dst: u16, src: u16, op: UnaryByteOp },
    /// Binary operator with optional D-027 outermost cancellation snap.
    Binary {
        dst: u16,
        lhs: u16,
        rhs: u16,
        op: BinaryOp,
        snap_zero: bool,
    },
    /// `ABS(src)`.
    Abs { dst: u16, src: u16 },
    /// `NOT(src)`.
    Not { dst: u16, src: u16 },
    /// `ROUND`, `ROUNDDOWN`, `ROUNDUP`, or `TRUNC`.
    Round {
        dst: u16,
        val: u16,
        digits: u16,
        mode: RoundingMode,
    },
    /// `SUM(fixed_area)` evaluated once per chunk.
    SumFixedArea { dst: u16, area_idx: usize },
    /// `AVERAGE(fixed_area)` evaluated once per chunk.
    AvgFixedArea { dst: u16, area_idx: usize },
    /// `MIN(fixed_area)` or `MAX(fixed_area)` evaluated once per chunk.
    MinMaxFixedArea {
        dst: u16,
        area_idx: usize,
        is_max: bool,
    },
    /// `COUNT(fixed_area)` or `COUNTA(fixed_area)` evaluated once per chunk.
    CountFixedArea {
        dst: u16,
        area_idx: usize,
        count_all: bool,
    },
    /// `AND(...)` or `OR(...)` over scalar expressions; stores `(reg, is_cell_ref)` pairs.
    AndOr {
        dst: u16,
        args: SmallVec<[(u16, bool); 4]>,
        is_or: bool,
    },
    /// Structured `IF(cond, then_branch, else_branch)` with per-chunk lazy branch execution.
    IfSelect {
        dst: u16,
        cond: u16,
        then_instrs: Vec<Instr>,
        then_reg: u16,
        else_instrs: Vec<Instr>,
        else_reg: u16,
    },
    /// Structured `IFERROR(primary, fallback)` or `IFNA(primary, fallback)` with lazy fallback.
    IfErrorSelect {
        dst: u16,
        primary: u16,
        fallback_instrs: Vec<Instr>,
        fallback_reg: u16,
        only_na: bool,
    },
    /// Exact `VLOOKUP(needle, fixed_area, col_index, FALSE)`.
    VlookupExact {
        dst: u16,
        needle: u16,
        area_idx: usize,
        col_index: u32,
    },
    /// Exact `MATCH(needle, fixed_area, 0)`.
    MatchExact {
        dst: u16,
        needle: u16,
        area_idx: usize,
    },
}

/// Compiled flat bytecode program and static dependency summary for a strip formula.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CompiledStripProgram {
    /// Formula evaluation mode (`Dynamic` or `Legacy`).
    pub mode: FormulaMode,
    /// Top-level instruction sequence.
    pub instrs: Vec<Instr>,
    /// Register index holding the root result of the formula.
    pub result_reg: u16,
    /// Total virtual registers allocated across top-level and branch blocks.
    pub num_regs: usize,
    /// Interned text literals referenced by `ConstText`.
    pub const_strings: Vec<String>,
    /// Deduplicated fixed cells read by the program.
    pub fixed_cells: Vec<(SheetId, RowId, ColId)>,
    /// Deduplicated non-self `stride == 1` columns read by the program.
    pub stride_cols: Vec<StrideColSpec>,
    /// Deduplicated fixed areas referenced by aggregation or lookup opcodes.
    pub fixed_areas: Vec<FixedAreaSpec>,
    /// Unique sheets touched by the host formula and its fixed/stride/area dependencies.
    pub touched_sheets: SmallVec<[SheetId; 4]>,
    /// `true` iff any `IfSelect` or `IfErrorSelect` short-circuit branch exists.
    pub has_lazy_branch: bool,
    /// `true` iff the program references `self` column at a strictly preceding row (`B5` pattern).
    pub sequential_self_dep: bool,
    /// Maximum back-row offset into `self` column (`0` if not self-dependent).
    pub max_self_back: u32,
    /// Lowest preceding `RowId` referenced directly as a back-self recurrence (`base_row` min).
    pub min_self_base_row: Option<u64>,
}

struct Compiler {
    host_sheet: SheetId,
    strip_col: ColId,
    start_row: RowId,
    strip_len: u32,
    allow_self_recurrence: bool,
    next_reg: usize,
    const_strings: Vec<String>,
    fixed_cells: Vec<(SheetId, RowId, ColId)>,
    stride_cols: Vec<StrideColSpec>,
    fixed_areas: Vec<FixedAreaSpec>,
    touched_sheets: SmallVec<[SheetId; 4]>,
    has_lazy_branch: bool,
    has_self_back: bool,
    max_self_back: u32,
    min_self_base_row: Option<u64>,
}

impl Compiler {
    fn new(
        host_sheet: SheetId,
        strip_col: ColId,
        start_row: RowId,
        strip_len: u32,
        allow_self_recurrence: bool,
    ) -> Self {
        let mut touched_sheets = SmallVec::new();
        touched_sheets.push(host_sheet);
        Self {
            host_sheet,
            strip_col,
            start_row,
            strip_len,
            allow_self_recurrence,
            next_reg: 0,
            const_strings: Vec::new(),
            fixed_cells: Vec::new(),
            stride_cols: Vec::new(),
            fixed_areas: Vec::new(),
            touched_sheets,
            has_lazy_branch: false,
            has_self_back: false,
            max_self_back: 0,
            min_self_base_row: None,
        }
    }

    fn alloc_reg(&mut self) -> Option<u16> {
        if self.next_reg >= MAX_VIRTUAL_REGS {
            return None;
        }
        let r = u16::try_from(self.next_reg).ok()?;
        self.next_reg = self.next_reg.saturating_add(1);
        Some(r)
    }

    fn touch_sheet(&mut self, sheet: SheetId) {
        if !self.touched_sheets.contains(&sheet) {
            self.touched_sheets.push(sheet);
        }
    }

    fn add_const_str(&mut self, s: &str) -> usize {
        if let Some(pos) = self.const_strings.iter().position(|x| x == s) {
            return pos;
        }
        let idx = self.const_strings.len();
        self.const_strings.push(String::from(s));
        idx
    }

    fn add_fixed_cell(&mut self, sheet: SheetId, row: RowId, col: ColId) {
        self.touch_sheet(sheet);
        let key = (sheet, row, col);
        if !self.fixed_cells.contains(&key) {
            self.fixed_cells.push(key);
        }
    }

    fn add_stride_col(&mut self, sheet: SheetId, base_row: u64, col: ColId) -> usize {
        self.touch_sheet(sheet);
        let spec = StrideColSpec {
            sheet,
            base_row,
            col,
        };
        if let Some(pos) = self.stride_cols.iter().position(|x| *x == spec) {
            return pos;
        }
        let idx = self.stride_cols.len();
        self.stride_cols.push(spec);
        idx
    }

    fn add_fixed_area(&mut self, area: FixedAreaSpec) -> usize {
        self.touch_sheet(area.sheet);
        if let Some(pos) = self.fixed_areas.iter().position(|x| *x == area) {
            return pos;
        }
        let idx = self.fixed_areas.len();
        self.fixed_areas.push(area);
        idx
    }

    #[expect(
        clippy::too_many_lines,
        reason = "central bytecode compiler expression matcher"
    )]
    fn compile_expr(
        &mut self,
        expr: &AffineExpr,
        is_outermost: bool,
        allow_missing: bool,
        depth: usize,
        out: &mut Vec<Instr>,
    ) -> Option<u16> {
        if depth > MAX_NESTING_DEPTH {
            return None;
        }
        let next_depth = depth.saturating_add(1);

        match expr {
            AffineExpr::Literal(lit) => {
                let dst = self.alloc_reg()?;
                match lit {
                    Literal::Number(n) => {
                        if !n.is_finite() {
                            out.push(Instr::ConstError {
                                dst,
                                err: CalcError::Num,
                            });
                        } else {
                            let val = if *n == 0.0 { 0.0 } else { *n };
                            out.push(Instr::ConstNum { dst, val });
                        }
                    }
                    Literal::Bool(val) => out.push(Instr::ConstBool { dst, val: *val }),
                    Literal::Text(s) => {
                        let const_str_idx = self.add_const_str(s);
                        out.push(Instr::ConstText { dst, const_str_idx });
                    }
                    Literal::Error(err) => out.push(Instr::ConstError { dst, err: *err }),
                }
                Some(dst)
            }
            AffineExpr::MissingArg => {
                if !allow_missing {
                    return None;
                }
                let dst = self.alloc_reg()?;
                out.push(Instr::ConstEmpty { dst });
                Some(dst)
            }
            AffineExpr::ArrayLiteral { .. }
            | AffineExpr::Name(_)
            | AffineExpr::ScopedName { .. }
            | AffineExpr::SpillRef(_) => None,
            // Explicit parentheses suppress outermost D-027 tail-zeroing.
            AffineExpr::Paren(inner) => self.compile_expr(inner, false, false, next_depth, out),
            AffineExpr::Unary { op, expr: inner } => {
                let byte_op = match op {
                    UnaryOp::Plus => UnaryByteOp::Plus,
                    UnaryOp::Neg => UnaryByteOp::Neg,
                    UnaryOp::Percent => UnaryByteOp::Percent,
                    // `@cell` keeps `Empty` distinct from `0.0` unlike `=cell`.
                    UnaryOp::ImplicitIntersection => return None,
                };
                let src = self.compile_expr(inner, false, false, next_depth, out)?;
                let dst = self.alloc_reg()?;
                out.push(Instr::Unary {
                    dst,
                    src,
                    op: byte_op,
                });
                Some(dst)
            }
            AffineExpr::Binary { op, left, right } => {
                let lhs = self.compile_expr(left, false, false, next_depth, out)?;
                let rhs = self.compile_expr(right, false, false, next_depth, out)?;
                let dst = self.alloc_reg()?;
                let snap_zero = is_outermost && matches!(op, BinaryOp::Add | BinaryOp::Sub);
                out.push(Instr::Binary {
                    dst,
                    lhs,
                    rhs,
                    op: *op,
                    snap_zero,
                });
                Some(dst)
            }
            AffineExpr::Reference(r) => match r {
                AffineRef::Cell {
                    sheet,
                    missing_sheet,
                    row,
                    col,
                    ..
                } => {
                    if missing_sheet.is_some() {
                        return None;
                    }
                    let target_sheet = sheet.unwrap_or(self.host_sheet);
                    if row.stride == 0 {
                        let target_row = RowId(row.base);
                        let strip_end = self.start_row.0.saturating_add(u64::from(self.strip_len));
                        if target_sheet == self.host_sheet
                            && *col == self.strip_col
                            && target_row.0 >= self.start_row.0
                            && target_row.0 < strip_end
                        {
                            return None;
                        }
                        self.add_fixed_cell(target_sheet, target_row, *col);
                        let dst = self.alloc_reg()?;
                        out.push(Instr::LoadFixedCell {
                            dst,
                            sheet: target_sheet,
                            row: target_row,
                            col: *col,
                        });
                        Some(dst)
                    } else if row.stride == 1 {
                        if target_sheet == self.host_sheet && *col == self.strip_col {
                            if !self.allow_self_recurrence || row.base >= self.start_row.0 {
                                return None;
                            }
                            let diff = self.start_row.0.checked_sub(row.base)?;
                            let back = u32::try_from(diff).ok()?;
                            if back == 0 {
                                return None;
                            }
                            self.has_self_back = true;
                            self.max_self_back = self.max_self_back.max(back);
                            self.min_self_base_row = Some(
                                self.min_self_base_row
                                    .map_or(row.base, |cur| cur.min(row.base)),
                            );
                            let dst = self.alloc_reg()?;
                            out.push(Instr::LoadStrideCell {
                                dst,
                                stride_idx: 0,
                                self_back_offset: Some(back),
                            });
                            Some(dst)
                        } else {
                            let stride_idx = self.add_stride_col(target_sheet, row.base, *col);
                            let dst = self.alloc_reg()?;
                            out.push(Instr::LoadStrideCell {
                                dst,
                                stride_idx,
                                self_back_offset: None,
                            });
                            Some(dst)
                        }
                    } else {
                        None
                    }
                }
                AffineRef::Area { .. }
                | AffineRef::ColRange { .. }
                | AffineRef::RowRange { .. }
                | AffineRef::Invalid { .. } => None,
            },
            AffineExpr::Call { name, args } => self.compile_call(name, args, next_depth, out),
        }
    }

    #[expect(clippy::too_many_lines, reason = "central built-in call compiler")]
    fn compile_call(
        &mut self,
        name: &str,
        args: &[AffineExpr],
        depth: usize,
        out: &mut Vec<Instr>,
    ) -> Option<u16> {
        match name {
            "TRUE" if args.is_empty() => {
                let dst = self.alloc_reg()?;
                out.push(Instr::ConstBool { dst, val: true });
                Some(dst)
            }
            "FALSE" if args.is_empty() => {
                let dst = self.alloc_reg()?;
                out.push(Instr::ConstBool { dst, val: false });
                Some(dst)
            }
            "NA" if args.is_empty() => {
                let dst = self.alloc_reg()?;
                out.push(Instr::ConstError {
                    dst,
                    err: CalcError::Na,
                });
                Some(dst)
            }
            "PI" if args.is_empty() => {
                let dst = self.alloc_reg()?;
                out.push(Instr::ConstNum {
                    dst,
                    val: core::f64::consts::PI,
                });
                Some(dst)
            }
            "ROW" if args.is_empty() => {
                let dst = self.alloc_reg()?;
                out.push(Instr::RowZeroArgs { dst });
                Some(dst)
            }
            "ABS" if args.len() == 1 => {
                let a0 = args.first()?;
                let src = self.compile_expr(a0, false, false, depth, out)?;
                let dst = self.alloc_reg()?;
                out.push(Instr::Abs { dst, src });
                Some(dst)
            }
            "NOT" if args.len() == 1 => {
                let a0 = args.first()?;
                let src = self.compile_expr(a0, false, false, depth, out)?;
                let dst = self.alloc_reg()?;
                out.push(Instr::Not { dst, src });
                Some(dst)
            }
            "ROUND" | "ROUNDDOWN" | "ROUNDUP" | "TRUNC" => {
                let mode = match name {
                    "ROUND" => RoundingMode::HalfAwayFromZero,
                    "ROUNDDOWN" | "TRUNC" => RoundingMode::TowardZero,
                    "ROUNDUP" => RoundingMode::AwayFromZero,
                    _ => return None,
                };
                if name == "TRUNC" && args.len() == 1 {
                    let a0 = args.first()?;
                    let val = self.compile_expr(a0, false, false, depth, out)?;
                    let digits = self.alloc_reg()?;
                    out.push(Instr::ConstNum {
                        dst: digits,
                        val: 0.0,
                    });
                    let dst = self.alloc_reg()?;
                    out.push(Instr::Round {
                        dst,
                        val,
                        digits,
                        mode,
                    });
                    return Some(dst);
                }
                if args.len() != 2 {
                    return None;
                }
                let (a0, a1) = (args.first()?, args.get(1)?);
                let val = self.compile_expr(a0, false, false, depth, out)?;
                let digits = self.compile_expr(a1, false, false, depth, out)?;
                let dst = self.alloc_reg()?;
                out.push(Instr::Round {
                    dst,
                    val,
                    digits,
                    mode,
                });
                Some(dst)
            }
            "SUM" | "AVERAGE" | "MIN" | "MAX" | "COUNT" | "COUNTA" if args.len() == 1 => {
                let spec = extract_fixed_area(args.first()?, self.host_sheet)?;
                let area_idx = self.add_fixed_area(spec);
                let dst = self.alloc_reg()?;
                let instr = match name {
                    "SUM" => Instr::SumFixedArea { dst, area_idx },
                    "AVERAGE" => Instr::AvgFixedArea { dst, area_idx },
                    "MIN" => Instr::MinMaxFixedArea {
                        dst,
                        area_idx,
                        is_max: false,
                    },
                    "MAX" => Instr::MinMaxFixedArea {
                        dst,
                        area_idx,
                        is_max: true,
                    },
                    "COUNT" => Instr::CountFixedArea {
                        dst,
                        area_idx,
                        count_all: false,
                    },
                    "COUNTA" => Instr::CountFixedArea {
                        dst,
                        area_idx,
                        count_all: true,
                    },
                    _ => return None,
                };
                out.push(instr);
                Some(dst)
            }
            "AND" | "OR" if !args.is_empty() && args.len() <= 8 => {
                let mut compiled_args: SmallVec<[(u16, bool); 4]> = SmallVec::new();
                for arg in args {
                    let is_cell_ref = matches!(arg, AffineExpr::Reference(AffineRef::Cell { .. }));
                    let reg = self.compile_expr(arg, false, false, depth, out)?;
                    compiled_args.push((reg, is_cell_ref));
                }
                let dst = self.alloc_reg()?;
                out.push(Instr::AndOr {
                    dst,
                    args: compiled_args,
                    is_or: name == "OR",
                });
                Some(dst)
            }
            "IF" if (2..=3).contains(&args.len()) => {
                self.has_lazy_branch = true;
                let cond = self.compile_expr(args.first()?, false, false, depth, out)?;
                let mut then_instrs = Vec::new();
                let then_reg =
                    self.compile_expr(args.get(1)?, false, true, depth, &mut then_instrs)?;
                let mut else_instrs = Vec::new();
                let else_reg = if let Some(else_expr) = args.get(2) {
                    self.compile_expr(else_expr, false, true, depth, &mut else_instrs)?
                } else {
                    let r = self.alloc_reg()?;
                    else_instrs.push(Instr::ConstBool { dst: r, val: false });
                    r
                };
                let dst = self.alloc_reg()?;
                out.push(Instr::IfSelect {
                    dst,
                    cond,
                    then_instrs,
                    then_reg,
                    else_instrs,
                    else_reg,
                });
                Some(dst)
            }
            "IFERROR" | "IFNA" if args.len() == 2 => {
                self.has_lazy_branch = true;
                let primary = self.compile_expr(args.first()?, false, false, depth, out)?;
                let mut fallback_instrs = Vec::new();
                let fallback_reg =
                    self.compile_expr(args.get(1)?, false, true, depth, &mut fallback_instrs)?;
                let dst = self.alloc_reg()?;
                out.push(Instr::IfErrorSelect {
                    dst,
                    primary,
                    fallback_instrs,
                    fallback_reg,
                    only_na: name == "IFNA",
                });
                Some(dst)
            }
            "VLOOKUP" if args.len() == 4 => {
                let area = extract_fixed_area(args.get(1)?, self.host_sheet)?;
                let col_index = extract_const_positive_u32(args.get(2)?)?;
                if !is_const_exact_false(args.get(3)?) {
                    return None;
                }
                let needle = self.compile_expr(args.first()?, false, false, depth, out)?;
                let area_idx = self.add_fixed_area(area);
                let dst = self.alloc_reg()?;
                out.push(Instr::VlookupExact {
                    dst,
                    needle,
                    area_idx,
                    col_index,
                });
                Some(dst)
            }
            "MATCH" if args.len() == 3 => {
                let area = extract_fixed_area(args.get(1)?, self.host_sheet)?;
                if !is_const_zero(args.get(2)?) {
                    return None;
                }
                let needle = self.compile_expr(args.first()?, false, false, depth, out)?;
                let area_idx = self.add_fixed_area(area);
                let dst = self.alloc_reg()?;
                out.push(Instr::MatchExact {
                    dst,
                    needle,
                    area_idx,
                });
                Some(dst)
            }
            _ => None,
        }
    }
}

fn extract_fixed_area(expr: &AffineExpr, host_sheet: SheetId) -> Option<FixedAreaSpec> {
    let AffineExpr::Reference(AffineRef::Area {
        sheet,
        missing_sheet,
        first_row,
        first_col,
        last_row,
        last_col,
        ..
    }) = expr
    else {
        return None;
    };
    if missing_sheet.is_some() || first_row.stride != 0 || last_row.stride != 0 {
        return None;
    }
    Some(FixedAreaSpec {
        sheet: sheet.unwrap_or(host_sheet),
        first_row: RowId(first_row.base),
        first_col: *first_col,
        last_row: RowId(last_row.base),
        last_col: *last_col,
    })
}

fn extract_const_positive_u32(expr: &AffineExpr) -> Option<u32> {
    match expr {
        AffineExpr::Literal(Literal::Number(n)) if n.is_finite() => {
            let t = num::trunc(*n);
            if t < 1.0 || t > f64::from(u32::MAX) {
                return None;
            }
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "range checked `1.0..=u32::MAX` after `trunc`"
            )]
            Some(t as u32)
        }
        AffineExpr::Literal(Literal::Bool(true)) => Some(1),
        _ => None,
    }
}

fn is_const_exact_false(expr: &AffineExpr) -> bool {
    match expr {
        AffineExpr::Literal(Literal::Bool(b)) => !*b,
        AffineExpr::Literal(Literal::Number(n)) => *n == 0.0,
        _ => false,
    }
}

fn is_const_zero(expr: &AffineExpr) -> bool {
    match expr {
        AffineExpr::Literal(Literal::Number(n)) if n.is_finite() => num::trunc(*n) == 0.0,
        AffineExpr::Literal(Literal::Bool(false)) => true,
        _ => false,
    }
}

/// Attempts to compile an [`AffineFormula`] into a chunked strip bytecode program.
/// Returns `None` if any AST node falls outside the supported batch instruction set.
#[must_use]
pub(crate) fn compile_strip_program(
    template: &AffineFormula,
    host_sheet: SheetId,
    strip_col: ColId,
    start_row: RowId,
    strip_len: u32,
    sequential_self_dep: bool,
) -> Option<CompiledStripProgram> {
    if strip_len < 2 {
        return None;
    }
    let mut compiler = Compiler::new(
        host_sheet,
        strip_col,
        start_row,
        strip_len,
        sequential_self_dep,
    );
    let mut instrs = Vec::new();
    let result_reg = compiler.compile_expr(&template.root, true, false, 0, &mut instrs)?;
    Some(CompiledStripProgram {
        mode: template.mode,
        instrs,
        result_reg,
        num_regs: compiler.next_reg,
        const_strings: compiler.const_strings,
        fixed_cells: compiler.fixed_cells,
        stride_cols: compiler.stride_cols,
        fixed_areas: compiler.fixed_areas,
        touched_sheets: compiler.touched_sheets,
        has_lazy_branch: compiler.has_lazy_branch,
        sequential_self_dep: compiler.has_self_back,
        max_self_back: compiler.max_self_back,
        min_self_base_row: compiler.min_self_base_row,
    })
}
