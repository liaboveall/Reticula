//! Validated bidirectional conversions between frozen [`crate::v1`] wire types
//! and in-memory `reticula_core` / `reticula_formula` structures.

use alloc::boxed::Box;
use alloc::vec::Vec;

use reticula_core::axis::AxisRun;
use reticula_core::format::{CurrencySymbol, NumberFormat};
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::ops::{
    CellEdit, CellInput, FormatPatch, FormatTarget, FormulaMode, MAX_CELL_TEXT_UTF16, Op,
    ScalarValue, SeedOrigin,
};
use reticula_core::style::{
    BorderSet, BorderWeight, CellStyle, ColorPatch, ColorValue, HorizontalAlign, StylePatch,
};
use reticula_core::value::CalcError;
use reticula_formula::ast::{BinaryOp, Literal, MAX_FUNCTION_ARGS, MAX_NESTING_DEPTH, UnaryOp};
use reticula_formula::binder::{
    AxisTarget, BoundExpr, BoundFormula, BoundRef, TemplateExpr, TemplateFormula, TemplateRef,
};

use crate::v1::{
    WireAxisRunV1, WireAxisTargetV1, WireBinaryOpV1, WireBorderSetV1, WireBorderWeightV1,
    WireBoundExprV1, WireBoundFormulaV1, WireBoundRefV1, WireCalcErrorV1, WireCellEditV1,
    WireCellInputV1, WireCellStyleV1, WireColorPatchV1, WireColorValueV1, WireCurrencySymbolV1,
    WireFormatPatchV1, WireFormatTargetV1, WireFormulaModeV1, WireHorizontalAlignV1, WireIdRunV1,
    WireLiteralV1, WireNumberFormatV1, WireOpV1, WireScalarValueV1, WireSeedOriginV1,
    WireStylePatchV1, WireTemplateExprV1, WireTemplateFormulaV1, WireTemplateRefV1, WireTxnV1,
    WireUnaryOpV1,
};

/// Errors produced when validating and converting untrusted v1 wire payloads
/// into in-memory kernel structures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireConvertError {
    /// Zero-length or overflowing `IdRun`.
    InvalidIdRun,
    /// Non-finite IEEE 754 float (`NaN` or `±Infinity`).
    NonFiniteFloat,
    /// Text exceeds `32,767` UTF-16 code units.
    TextTooLong,
    /// Out-of-bounds number format decimals or color palette/RGB value.
    InvalidFormatOrStyle,
    /// Formula AST exceeds maximum nesting depth (`64`) or argument count (`255`).
    FormulaLimitExceeded,
    /// Array literal dimensions do not match element count or equal zero.
    InvalidArrayLiteral,
    /// Operation variant introduced in wire v2+ cannot be encoded as [`WireOpV1`].
    UnsupportedInV1,
    /// Operation or formula variant introduced in wire v3+ cannot be encoded as v2.
    UnsupportedInV2,
    /// State digest version is unsupported.
    UnsupportedDigestVersion,
}

pub(crate) fn canon_f64(n: f64) -> Result<f64, WireConvertError> {
    if !n.is_finite() {
        return Err(WireConvertError::NonFiniteFloat);
    }
    Ok(if n == 0.0 { 0.0 } else { n })
}

pub(crate) fn check_text_len(s: &str) -> Result<(), WireConvertError> {
    if s.encode_utf16().count() > MAX_CELL_TEXT_UTF16 {
        return Err(WireConvertError::TextTooLong);
    }
    Ok(())
}

// ------------------------------------------------------------ IdRun & AxisRun

impl From<IdRun> for WireIdRunV1 {
    fn from(r: IdRun) -> Self {
        Self {
            first_id: r.first_id,
            len: r.len,
        }
    }
}

impl TryFrom<WireIdRunV1> for IdRun {
    type Error = WireConvertError;

    fn try_from(w: WireIdRunV1) -> Result<Self, Self::Error> {
        Self::new(w.first_id, w.len).ok_or(WireConvertError::InvalidIdRun)
    }
}

impl From<&AxisRun> for WireAxisRunV1 {
    fn from(r: &AxisRun) -> Self {
        Self {
            first_id: r.ids.first_id,
            len: r.ids.len,
            tombstones: r.tombstones.as_ref().map(|t| t.to_vec()),
        }
    }
}

impl TryFrom<WireAxisRunV1> for AxisRun {
    type Error = WireConvertError;

    fn try_from(w: WireAxisRunV1) -> Result<Self, WireConvertError> {
        let ids = IdRun::new(w.first_id, w.len).ok_or(WireConvertError::InvalidIdRun)?;
        match w.tombstones {
            None => Ok(Self {
                ids,
                live_count: w.len,
                tombstones: None,
            }),
            Some(tombs) => {
                if tombs.len() != w.len as usize {
                    return Err(WireConvertError::InvalidIdRun);
                }
                let mut live_count = 0_u32;
                for &c in &tombs {
                    if c == 0 {
                        live_count = live_count.saturating_add(1);
                    }
                }
                if live_count == w.len {
                    Ok(Self {
                        ids,
                        live_count: w.len,
                        tombstones: None,
                    })
                } else {
                    Ok(Self {
                        ids,
                        live_count,
                        tombstones: Some(tombs.into_boxed_slice()),
                    })
                }
            }
        }
    }
}

pub(crate) fn runs_to_wire(runs: &[IdRun]) -> Vec<WireIdRunV1> {
    runs.iter().copied().map(WireIdRunV1::from).collect()
}

pub(crate) fn runs_from_wire(runs: Vec<WireIdRunV1>) -> Result<Vec<IdRun>, WireConvertError> {
    runs.into_iter().map(IdRun::try_from).collect()
}

// ------------------------------------------------------------ SeedOrigin

impl From<SeedOrigin> for WireSeedOriginV1 {
    fn from(o: SeedOrigin) -> Self {
        Self {
            seed: o.seed,
            rows: o.rows,
            generator_version: o.generator_version,
        }
    }
}

impl From<WireSeedOriginV1> for SeedOrigin {
    fn from(w: WireSeedOriginV1) -> Self {
        Self {
            seed: w.seed,
            rows: w.rows,
            generator_version: w.generator_version,
        }
    }
}

// ------------------------------------------------------------ Errors & Scalars

impl From<CalcError> for WireCalcErrorV1 {
    fn from(e: CalcError) -> Self {
        match e {
            CalcError::Div0 => Self::Div0,
            CalcError::Value => Self::Value,
            CalcError::Ref => Self::Ref,
            CalcError::Name => Self::Name,
            CalcError::Num => Self::Num,
            CalcError::Na => Self::Na,
            CalcError::Null => Self::Null,
            CalcError::Spill => Self::Spill,
            CalcError::Calc => Self::Calc,
            CalcError::Cycle => Self::Cycle,
        }
    }
}

impl From<WireCalcErrorV1> for CalcError {
    fn from(w: WireCalcErrorV1) -> Self {
        match w {
            WireCalcErrorV1::Div0 => Self::Div0,
            WireCalcErrorV1::Value => Self::Value,
            WireCalcErrorV1::Ref => Self::Ref,
            WireCalcErrorV1::Name => Self::Name,
            WireCalcErrorV1::Num => Self::Num,
            WireCalcErrorV1::Na => Self::Na,
            WireCalcErrorV1::Null => Self::Null,
            WireCalcErrorV1::Spill => Self::Spill,
            WireCalcErrorV1::Calc => Self::Calc,
            WireCalcErrorV1::Cycle => Self::Cycle,
        }
    }
}

impl From<&ScalarValue> for WireScalarValueV1 {
    fn from(v: &ScalarValue) -> Self {
        match v {
            ScalarValue::Number(n) => Self::Number(if *n == 0.0 { 0.0 } else { *n }),
            ScalarValue::Text(s) => Self::Text(s.clone()),
            ScalarValue::Bool(b) => Self::Bool(*b),
            ScalarValue::Error(e) => Self::Error((*e).into()),
        }
    }
}

impl TryFrom<WireScalarValueV1> for ScalarValue {
    type Error = WireConvertError;

    fn try_from(w: WireScalarValueV1) -> Result<Self, WireConvertError> {
        match w {
            WireScalarValueV1::Number(n) => Ok(Self::Number(canon_f64(n)?)),
            WireScalarValueV1::Text(s) => {
                check_text_len(&s)?;
                Ok(Self::Text(s))
            }
            WireScalarValueV1::Bool(b) => Ok(Self::Bool(b)),
            WireScalarValueV1::Error(e) => Ok(Self::Error(e.into())),
        }
    }
}

// ------------------------------------------------------------ Formats & Styles

impl From<CurrencySymbol> for WireCurrencySymbolV1 {
    fn from(s: CurrencySymbol) -> Self {
        match s {
            CurrencySymbol::Usd => Self::Usd,
            CurrencySymbol::Cny => Self::Cny,
            CurrencySymbol::Eur => Self::Eur,
        }
    }
}

impl From<WireCurrencySymbolV1> for CurrencySymbol {
    fn from(w: WireCurrencySymbolV1) -> Self {
        match w {
            WireCurrencySymbolV1::Usd => Self::Usd,
            WireCurrencySymbolV1::Cny => Self::Cny,
            WireCurrencySymbolV1::Eur => Self::Eur,
        }
    }
}

impl From<NumberFormat> for WireNumberFormatV1 {
    fn from(f: NumberFormat) -> Self {
        match f.normalize() {
            NumberFormat::General => Self::General,
            NumberFormat::Fixed2 => Self::Fixed2,
            NumberFormat::Percent => Self::Percent,
            NumberFormat::Date => Self::Date,
            NumberFormat::Integer => Self::Integer,
            NumberFormat::Fixed {
                decimals,
                thousands,
            } => Self::Fixed {
                decimals,
                thousands,
            },
            NumberFormat::Currency { symbol, decimals } => Self::Currency {
                symbol: symbol.into(),
                decimals,
            },
            NumberFormat::PercentDecimals { decimals } => Self::PercentDecimals { decimals },
            NumberFormat::Scientific { decimals } => Self::Scientific { decimals },
            NumberFormat::DateSlash => Self::DateSlash,
            NumberFormat::DateUs => Self::DateUs,
            NumberFormat::DateYearMonth => Self::DateYearMonth,
            NumberFormat::DateTime { seconds } => Self::DateTime { seconds },
            NumberFormat::Time { seconds } => Self::Time { seconds },
            NumberFormat::PlainText => Self::PlainText,
        }
    }
}

impl TryFrom<WireNumberFormatV1> for NumberFormat {
    type Error = WireConvertError;

    fn try_from(w: WireNumberFormatV1) -> Result<Self, Self::Error> {
        let fmt = match w {
            WireNumberFormatV1::General => Self::General,
            WireNumberFormatV1::Fixed2 => Self::Fixed2,
            WireNumberFormatV1::Percent => Self::Percent,
            WireNumberFormatV1::Date => Self::Date,
            WireNumberFormatV1::Integer => Self::Integer,
            WireNumberFormatV1::Fixed {
                decimals,
                thousands,
            } => Self::Fixed {
                decimals,
                thousands,
            },
            WireNumberFormatV1::Currency { symbol, decimals } => Self::Currency {
                symbol: symbol.into(),
                decimals,
            },
            WireNumberFormatV1::PercentDecimals { decimals } => Self::PercentDecimals { decimals },
            WireNumberFormatV1::Scientific { decimals } => Self::Scientific { decimals },
            WireNumberFormatV1::DateSlash => Self::DateSlash,
            WireNumberFormatV1::DateUs => Self::DateUs,
            WireNumberFormatV1::DateYearMonth => Self::DateYearMonth,
            WireNumberFormatV1::DateTime { seconds } => Self::DateTime { seconds },
            WireNumberFormatV1::Time { seconds } => Self::Time { seconds },
            WireNumberFormatV1::PlainText => Self::PlainText,
        };
        if !fmt.is_valid() {
            return Err(WireConvertError::InvalidFormatOrStyle);
        }
        Ok(fmt.normalize())
    }
}

impl From<FormatPatch> for WireFormatPatchV1 {
    fn from(p: FormatPatch) -> Self {
        match p {
            FormatPatch::Keep => Self::Keep,
            FormatPatch::Set(f) => Self::Set(f.into()),
            FormatPatch::Clear => Self::Clear,
        }
    }
}

impl TryFrom<WireFormatPatchV1> for FormatPatch {
    type Error = WireConvertError;

    fn try_from(w: WireFormatPatchV1) -> Result<Self, Self::Error> {
        match w {
            WireFormatPatchV1::Keep => Ok(Self::Keep),
            WireFormatPatchV1::Set(f) => Ok(Self::Set(f.try_into()?)),
            WireFormatPatchV1::Clear => Ok(Self::Clear),
        }
    }
}

impl From<&FormatTarget> for WireFormatTargetV1 {
    fn from(t: &FormatTarget) -> Self {
        match t {
            FormatTarget::Cells { rows, cols } => Self::Cells {
                rows: runs_to_wire(rows),
                cols: runs_to_wire(cols),
            },
            FormatTarget::Rows(rows) => Self::Rows(runs_to_wire(rows)),
            FormatTarget::Cols(cols) => Self::Cols(runs_to_wire(cols)),
        }
    }
}

impl TryFrom<WireFormatTargetV1> for FormatTarget {
    type Error = WireConvertError;

    fn try_from(w: WireFormatTargetV1) -> Result<Self, Self::Error> {
        match w {
            WireFormatTargetV1::Cells { rows, cols } => Ok(Self::Cells {
                rows: runs_from_wire(rows)?,
                cols: runs_from_wire(cols)?,
            }),
            WireFormatTargetV1::Rows(rows) => Ok(Self::Rows(runs_from_wire(rows)?)),
            WireFormatTargetV1::Cols(cols) => Ok(Self::Cols(runs_from_wire(cols)?)),
        }
    }
}

impl From<HorizontalAlign> for WireHorizontalAlignV1 {
    fn from(a: HorizontalAlign) -> Self {
        match a {
            HorizontalAlign::Auto => Self::Auto,
            HorizontalAlign::Left => Self::Left,
            HorizontalAlign::Center => Self::Center,
            HorizontalAlign::Right => Self::Right,
        }
    }
}

impl From<WireHorizontalAlignV1> for HorizontalAlign {
    fn from(w: WireHorizontalAlignV1) -> Self {
        match w {
            WireHorizontalAlignV1::Auto => Self::Auto,
            WireHorizontalAlignV1::Left => Self::Left,
            WireHorizontalAlignV1::Center => Self::Center,
            WireHorizontalAlignV1::Right => Self::Right,
        }
    }
}

impl From<ColorValue> for WireColorValueV1 {
    fn from(c: ColorValue) -> Self {
        match c {
            ColorValue::Palette(s) => Self::Palette(s),
            ColorValue::Rgb(rgb) => Self::Rgb(rgb),
        }
    }
}

impl TryFrom<WireColorValueV1> for ColorValue {
    type Error = WireConvertError;

    fn try_from(w: WireColorValueV1) -> Result<Self, Self::Error> {
        let c = match w {
            WireColorValueV1::Palette(s) => Self::Palette(s),
            WireColorValueV1::Rgb(rgb) => Self::Rgb(rgb),
        };
        if !c.is_valid() {
            return Err(WireConvertError::InvalidFormatOrStyle);
        }
        Ok(c)
    }
}

impl From<ColorPatch> for WireColorPatchV1 {
    fn from(p: ColorPatch) -> Self {
        match p {
            ColorPatch::Keep => Self::Keep,
            ColorPatch::Set(c) => Self::Set(c.into()),
            ColorPatch::Clear => Self::Clear,
        }
    }
}

impl TryFrom<WireColorPatchV1> for ColorPatch {
    type Error = WireConvertError;

    fn try_from(w: WireColorPatchV1) -> Result<Self, Self::Error> {
        match w {
            WireColorPatchV1::Keep => Ok(Self::Keep),
            WireColorPatchV1::Set(c) => Ok(Self::Set(c.try_into()?)),
            WireColorPatchV1::Clear => Ok(Self::Clear),
        }
    }
}

impl From<BorderWeight> for WireBorderWeightV1 {
    fn from(b: BorderWeight) -> Self {
        match b {
            BorderWeight::None => Self::None,
            BorderWeight::Thin => Self::Thin,
            BorderWeight::Medium => Self::Medium,
        }
    }
}

impl From<WireBorderWeightV1> for BorderWeight {
    fn from(w: WireBorderWeightV1) -> Self {
        match w {
            WireBorderWeightV1::None => Self::None,
            WireBorderWeightV1::Thin => Self::Thin,
            WireBorderWeightV1::Medium => Self::Medium,
        }
    }
}

impl From<BorderSet> for WireBorderSetV1 {
    fn from(b: BorderSet) -> Self {
        Self {
            top: b.top.into(),
            right: b.right.into(),
            bottom: b.bottom.into(),
            left: b.left.into(),
        }
    }
}

impl From<WireBorderSetV1> for BorderSet {
    fn from(w: WireBorderSetV1) -> Self {
        Self {
            top: w.top.into(),
            right: w.right.into(),
            bottom: w.bottom.into(),
            left: w.left.into(),
        }
    }
}

impl From<CellStyle> for WireCellStyleV1 {
    fn from(s: CellStyle) -> Self {
        Self {
            bold: s.bold,
            italic: s.italic,
            strikethrough: s.strikethrough,
            align: s.align.into(),
            text_color: s.text_color.map(WireColorValueV1::from),
            fill_color: s.fill_color.map(WireColorValueV1::from),
            borders: s.borders.into(),
        }
    }
}

impl TryFrom<WireCellStyleV1> for CellStyle {
    type Error = WireConvertError;

    fn try_from(w: WireCellStyleV1) -> Result<Self, Self::Error> {
        let style = Self {
            bold: w.bold,
            italic: w.italic,
            strikethrough: w.strikethrough,
            align: w.align.into(),
            text_color: match w.text_color {
                Some(c) => Some(c.try_into()?),
                None => None,
            },
            fill_color: match w.fill_color {
                Some(c) => Some(c.try_into()?),
                None => None,
            },
            borders: w.borders.into(),
        };
        if !style.is_valid() {
            return Err(WireConvertError::InvalidFormatOrStyle);
        }
        Ok(style)
    }
}

impl From<StylePatch> for WireStylePatchV1 {
    fn from(p: StylePatch) -> Self {
        Self {
            bold: p.bold,
            italic: p.italic,
            strikethrough: p.strikethrough,
            align: p.align.map(WireHorizontalAlignV1::from),
            text_color: p.text_color.into(),
            fill_color: p.fill_color.into(),
            border_top: p.border_top.map(WireBorderWeightV1::from),
            border_right: p.border_right.map(WireBorderWeightV1::from),
            border_bottom: p.border_bottom.map(WireBorderWeightV1::from),
            border_left: p.border_left.map(WireBorderWeightV1::from),
        }
    }
}

impl TryFrom<WireStylePatchV1> for StylePatch {
    type Error = WireConvertError;

    fn try_from(w: WireStylePatchV1) -> Result<Self, Self::Error> {
        let patch = Self {
            bold: w.bold,
            italic: w.italic,
            strikethrough: w.strikethrough,
            align: w.align.map(HorizontalAlign::from),
            text_color: w.text_color.try_into()?,
            fill_color: w.fill_color.try_into()?,
            border_top: w.border_top.map(BorderWeight::from),
            border_right: w.border_right.map(BorderWeight::from),
            border_bottom: w.border_bottom.map(BorderWeight::from),
            border_left: w.border_left.map(BorderWeight::from),
        };
        if !patch.is_valid() {
            return Err(WireConvertError::InvalidFormatOrStyle);
        }
        Ok(patch)
    }
}

// ------------------------------------------------------------ Formulas

impl From<FormulaMode> for WireFormulaModeV1 {
    fn from(m: FormulaMode) -> Self {
        match m {
            FormulaMode::Dynamic => Self::Dynamic,
            FormulaMode::Legacy => Self::Legacy,
        }
    }
}

impl From<WireFormulaModeV1> for FormulaMode {
    fn from(w: WireFormulaModeV1) -> Self {
        match w {
            WireFormulaModeV1::Dynamic => Self::Dynamic,
            WireFormulaModeV1::Legacy => Self::Legacy,
        }
    }
}

impl From<UnaryOp> for WireUnaryOpV1 {
    fn from(op: UnaryOp) -> Self {
        match op {
            UnaryOp::Plus => Self::Plus,
            UnaryOp::Neg => Self::Neg,
            UnaryOp::ImplicitIntersection => Self::ImplicitIntersection,
            UnaryOp::Percent => Self::Percent,
        }
    }
}

impl From<WireUnaryOpV1> for UnaryOp {
    fn from(w: WireUnaryOpV1) -> Self {
        match w {
            WireUnaryOpV1::Plus => Self::Plus,
            WireUnaryOpV1::Neg => Self::Neg,
            WireUnaryOpV1::ImplicitIntersection => Self::ImplicitIntersection,
            WireUnaryOpV1::Percent => Self::Percent,
        }
    }
}

impl From<BinaryOp> for WireBinaryOpV1 {
    fn from(op: BinaryOp) -> Self {
        match op {
            BinaryOp::Pow => Self::Pow,
            BinaryOp::Mul => Self::Mul,
            BinaryOp::Div => Self::Div,
            BinaryOp::Add => Self::Add,
            BinaryOp::Sub => Self::Sub,
            BinaryOp::Concat => Self::Concat,
            BinaryOp::Eq => Self::Eq,
            BinaryOp::Ne => Self::Ne,
            BinaryOp::Lt => Self::Lt,
            BinaryOp::Gt => Self::Gt,
            BinaryOp::Le => Self::Le,
            BinaryOp::Ge => Self::Ge,
        }
    }
}

impl From<WireBinaryOpV1> for BinaryOp {
    fn from(w: WireBinaryOpV1) -> Self {
        match w {
            WireBinaryOpV1::Pow => Self::Pow,
            WireBinaryOpV1::Mul => Self::Mul,
            WireBinaryOpV1::Div => Self::Div,
            WireBinaryOpV1::Add => Self::Add,
            WireBinaryOpV1::Sub => Self::Sub,
            WireBinaryOpV1::Concat => Self::Concat,
            WireBinaryOpV1::Eq => Self::Eq,
            WireBinaryOpV1::Ne => Self::Ne,
            WireBinaryOpV1::Lt => Self::Lt,
            WireBinaryOpV1::Gt => Self::Gt,
            WireBinaryOpV1::Le => Self::Le,
            WireBinaryOpV1::Ge => Self::Ge,
        }
    }
}

impl From<&Literal> for WireLiteralV1 {
    fn from(lit: &Literal) -> Self {
        match lit {
            Literal::Number(n) => Self::Number(if *n == 0.0 { 0.0 } else { *n }),
            Literal::Text(s) => Self::Text(s.clone()),
            Literal::Bool(b) => Self::Bool(*b),
            Literal::Error(e) => Self::Error((*e).into()),
        }
    }
}

impl TryFrom<WireLiteralV1> for Literal {
    type Error = WireConvertError;

    fn try_from(w: WireLiteralV1) -> Result<Self, WireConvertError> {
        match w {
            WireLiteralV1::Number(n) => Ok(Self::Number(canon_f64(n)?)),
            WireLiteralV1::Text(s) => {
                check_text_len(&s)?;
                Ok(Self::Text(s))
            }
            WireLiteralV1::Bool(b) => Ok(Self::Bool(b)),
            WireLiteralV1::Error(e) => Ok(Self::Error(e.into())),
        }
    }
}

impl From<&BoundRef> for WireBoundRefV1 {
    fn from(r: &BoundRef) -> Self {
        match r {
            BoundRef::Cell {
                sheet,
                missing_sheet,
                row,
                col,
                abs_col,
                abs_row,
            } => Self::Cell {
                sheet: sheet.map(|s| s.0),
                missing_sheet: missing_sheet.clone(),
                row: row.0,
                col: col.0,
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
            } => Self::Area {
                sheet: sheet.map(|s| s.0),
                missing_sheet: missing_sheet.clone(),
                first_row: first_row.0,
                first_col: first_col.0,
                last_row: last_row.0,
                last_col: last_col.0,
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
            } => Self::ColRange {
                sheet: sheet.map(|s| s.0),
                missing_sheet: missing_sheet.clone(),
                first_col: first_col.0,
                last_col: last_col.0,
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
            } => Self::RowRange {
                sheet: sheet.map(|s| s.0),
                missing_sheet: missing_sheet.clone(),
                first_row: first_row.0,
                last_row: last_row.0,
                abs_first: *abs_first,
                abs_last: *abs_last,
            },
            BoundRef::Invalid { sheet_name } => Self::Invalid {
                sheet_name: sheet_name.clone(),
            },
        }
    }
}

impl From<WireBoundRefV1> for BoundRef {
    fn from(w: WireBoundRefV1) -> Self {
        match w {
            WireBoundRefV1::Cell {
                sheet,
                missing_sheet,
                row,
                col,
                abs_col,
                abs_row,
            } => Self::Cell {
                sheet: sheet.map(SheetId),
                missing_sheet,
                row: RowId(row),
                col: ColId(col),
                abs_col,
                abs_row,
            },
            WireBoundRefV1::Area {
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
            } => Self::Area {
                sheet: sheet.map(SheetId),
                missing_sheet,
                first_row: RowId(first_row),
                first_col: ColId(first_col),
                last_row: RowId(last_row),
                last_col: ColId(last_col),
                abs_first_col,
                abs_first_row,
                abs_last_col,
                abs_last_row,
            },
            WireBoundRefV1::ColRange {
                sheet,
                missing_sheet,
                first_col,
                last_col,
                abs_first,
                abs_last,
            } => Self::ColRange {
                sheet: sheet.map(SheetId),
                missing_sheet,
                first_col: ColId(first_col),
                last_col: ColId(last_col),
                abs_first,
                abs_last,
            },
            WireBoundRefV1::RowRange {
                sheet,
                missing_sheet,
                first_row,
                last_row,
                abs_first,
                abs_last,
            } => Self::RowRange {
                sheet: sheet.map(SheetId),
                missing_sheet,
                first_row: RowId(first_row),
                last_row: RowId(last_row),
                abs_first,
                abs_last,
            },
            WireBoundRefV1::Invalid { sheet_name } => Self::Invalid { sheet_name },
        }
    }
}

fn bound_expr_to_wire(expr: &BoundExpr) -> Result<WireBoundExprV1, WireConvertError> {
    match expr {
        BoundExpr::Literal(lit) => Ok(WireBoundExprV1::Literal(lit.into())),
        BoundExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => Ok(WireBoundExprV1::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.iter().map(WireLiteralV1::from).collect(),
        }),
        BoundExpr::Reference(r) => Ok(WireBoundExprV1::Reference(r.into())),
        BoundExpr::Name(n) => Ok(WireBoundExprV1::Name(n.clone())),
        BoundExpr::Paren(inner) => Ok(WireBoundExprV1::Paren(Box::new(bound_expr_to_wire(inner)?))),
        BoundExpr::Unary { op, expr } => Ok(WireBoundExprV1::Unary {
            op: (*op).into(),
            expr: Box::new(bound_expr_to_wire(expr)?),
        }),
        BoundExpr::Binary { op, left, right } => Ok(WireBoundExprV1::Binary {
            op: (*op).into(),
            left: Box::new(bound_expr_to_wire(left)?),
            right: Box::new(bound_expr_to_wire(right)?),
        }),
        BoundExpr::Call { name, args } => {
            let mut out = Vec::with_capacity(args.len());
            for arg in args {
                out.push(bound_expr_to_wire(arg)?);
            }
            Ok(WireBoundExprV1::Call {
                name: name.clone(),
                args: out,
            })
        }
        BoundExpr::ScopedName { .. }
        | BoundExpr::SpillRef(_)
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => Err(WireConvertError::UnsupportedInV1),
    }
}

fn bound_expr_from_wire(w: WireBoundExprV1, depth: usize) -> Result<BoundExpr, WireConvertError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(WireConvertError::FormulaLimitExceeded);
    }
    let next = depth.saturating_add(1);
    match w {
        WireBoundExprV1::Literal(lit) => Ok(BoundExpr::Literal(lit.try_into()?)),
        WireBoundExprV1::ArrayLiteral {
            rows,
            cols,
            elements,
        } => {
            if rows == 0 || cols == 0 {
                return Err(WireConvertError::InvalidArrayLiteral);
            }
            let expected = (rows as usize)
                .checked_mul(cols as usize)
                .ok_or(WireConvertError::InvalidArrayLiteral)?;
            if expected != elements.len() {
                return Err(WireConvertError::InvalidArrayLiteral);
            }
            let converted: Result<Vec<Literal>, WireConvertError> =
                elements.into_iter().map(Literal::try_from).collect();
            Ok(BoundExpr::ArrayLiteral {
                rows,
                cols,
                elements: converted?,
            })
        }
        WireBoundExprV1::Reference(r) => Ok(BoundExpr::Reference(r.into())),
        WireBoundExprV1::Name(n) => {
            check_text_len(&n)?;
            Ok(BoundExpr::Name(n))
        }
        WireBoundExprV1::Paren(inner) => Ok(BoundExpr::Paren(Box::new(bound_expr_from_wire(
            *inner, next,
        )?))),
        WireBoundExprV1::Unary { op, expr } => Ok(BoundExpr::Unary {
            op: op.into(),
            expr: Box::new(bound_expr_from_wire(*expr, next)?),
        }),
        WireBoundExprV1::Binary { op, left, right } => Ok(BoundExpr::Binary {
            op: op.into(),
            left: Box::new(bound_expr_from_wire(*left, next)?),
            right: Box::new(bound_expr_from_wire(*right, next)?),
        }),
        WireBoundExprV1::Call { name, args } => {
            if args.len() > MAX_FUNCTION_ARGS {
                return Err(WireConvertError::FormulaLimitExceeded);
            }
            check_text_len(&name)?;
            let mut out = Vec::with_capacity(args.len());
            for arg in args {
                out.push(bound_expr_from_wire(arg, next)?);
            }
            Ok(BoundExpr::Call {
                name: name.to_ascii_uppercase(),
                args: out,
            })
        }
    }
}

impl TryFrom<&BoundFormula> for WireBoundFormulaV1 {
    type Error = WireConvertError;

    fn try_from(f: &BoundFormula) -> Result<Self, Self::Error> {
        Ok(Self {
            mode: f.mode.into(),
            root: bound_expr_to_wire(&f.root)?,
        })
    }
}

impl TryFrom<WireBoundFormulaV1> for BoundFormula {
    type Error = WireConvertError;

    fn try_from(w: WireBoundFormulaV1) -> Result<Self, Self::Error> {
        Ok(Self {
            mode: w.mode.into(),
            root: bound_expr_from_wire(w.root, 0)?,
        })
    }
}

// ------------------------------------------------------------ TemplateFormula

impl From<AxisTarget> for WireAxisTargetV1 {
    fn from(t: AxisTarget) -> Self {
        match t {
            AxisTarget::Fixed(id) => Self::Fixed(id),
            AxisTarget::IdDelta(d) => Self::IdDelta(d),
        }
    }
}

impl From<WireAxisTargetV1> for AxisTarget {
    fn from(w: WireAxisTargetV1) -> Self {
        match w {
            WireAxisTargetV1::Fixed(id) => Self::Fixed(id),
            WireAxisTargetV1::IdDelta(d) => Self::IdDelta(d),
        }
    }
}

impl From<&TemplateRef> for WireTemplateRefV1 {
    fn from(r: &TemplateRef) -> Self {
        match r {
            TemplateRef::Cell {
                sheet,
                missing_sheet,
                row,
                col,
                abs_col,
                abs_row,
            } => Self::Cell {
                sheet: sheet.map(|s| s.0),
                missing_sheet: missing_sheet.clone(),
                row: (*row).into(),
                col: (*col).into(),
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
            } => Self::Area {
                sheet: sheet.map(|s| s.0),
                missing_sheet: missing_sheet.clone(),
                first_row: (*first_row).into(),
                first_col: (*first_col).into(),
                last_row: (*last_row).into(),
                last_col: (*last_col).into(),
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
            } => Self::ColRange {
                sheet: sheet.map(|s| s.0),
                missing_sheet: missing_sheet.clone(),
                first_col: (*first_col).into(),
                last_col: (*last_col).into(),
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
            } => Self::RowRange {
                sheet: sheet.map(|s| s.0),
                missing_sheet: missing_sheet.clone(),
                first_row: (*first_row).into(),
                last_row: (*last_row).into(),
                abs_first: *abs_first,
                abs_last: *abs_last,
            },
            TemplateRef::Invalid { sheet_name } => Self::Invalid {
                sheet_name: sheet_name.clone(),
            },
        }
    }
}

impl From<WireTemplateRefV1> for TemplateRef {
    fn from(w: WireTemplateRefV1) -> Self {
        match w {
            WireTemplateRefV1::Cell {
                sheet,
                missing_sheet,
                row,
                col,
                abs_col,
                abs_row,
            } => Self::Cell {
                sheet: sheet.map(SheetId),
                missing_sheet,
                row: row.into(),
                col: col.into(),
                abs_col,
                abs_row,
            },
            WireTemplateRefV1::Area {
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
            } => Self::Area {
                sheet: sheet.map(SheetId),
                missing_sheet,
                first_row: first_row.into(),
                first_col: first_col.into(),
                last_row: last_row.into(),
                last_col: last_col.into(),
                abs_first_col,
                abs_first_row,
                abs_last_col,
                abs_last_row,
            },
            WireTemplateRefV1::ColRange {
                sheet,
                missing_sheet,
                first_col,
                last_col,
                abs_first,
                abs_last,
            } => Self::ColRange {
                sheet: sheet.map(SheetId),
                missing_sheet,
                first_col: first_col.into(),
                last_col: last_col.into(),
                abs_first,
                abs_last,
            },
            WireTemplateRefV1::RowRange {
                sheet,
                missing_sheet,
                first_row,
                last_row,
                abs_first,
                abs_last,
            } => Self::RowRange {
                sheet: sheet.map(SheetId),
                missing_sheet,
                first_row: first_row.into(),
                last_row: last_row.into(),
                abs_first,
                abs_last,
            },
            WireTemplateRefV1::Invalid { sheet_name } => Self::Invalid { sheet_name },
        }
    }
}

fn template_expr_to_wire(expr: &TemplateExpr) -> Result<WireTemplateExprV1, WireConvertError> {
    match expr {
        TemplateExpr::Literal(lit) => Ok(WireTemplateExprV1::Literal(lit.into())),
        TemplateExpr::ArrayLiteral {
            rows,
            cols,
            elements,
        } => Ok(WireTemplateExprV1::ArrayLiteral {
            rows: *rows,
            cols: *cols,
            elements: elements.iter().map(WireLiteralV1::from).collect(),
        }),
        TemplateExpr::Reference(r) => Ok(WireTemplateExprV1::Reference(r.into())),
        TemplateExpr::Name(n) => Ok(WireTemplateExprV1::Name(n.clone())),
        TemplateExpr::Paren(inner) => Ok(WireTemplateExprV1::Paren(Box::new(
            template_expr_to_wire(inner)?,
        ))),
        TemplateExpr::Unary { op, expr } => Ok(WireTemplateExprV1::Unary {
            op: (*op).into(),
            expr: Box::new(template_expr_to_wire(expr)?),
        }),
        TemplateExpr::Binary { op, left, right } => Ok(WireTemplateExprV1::Binary {
            op: (*op).into(),
            left: Box::new(template_expr_to_wire(left)?),
            right: Box::new(template_expr_to_wire(right)?),
        }),
        TemplateExpr::Call { name, args } => {
            let mut out = Vec::with_capacity(args.len());
            for arg in args {
                out.push(template_expr_to_wire(arg)?);
            }
            Ok(WireTemplateExprV1::Call {
                name: name.clone(),
                args: out,
            })
        }
        TemplateExpr::ScopedName { .. }
        | TemplateExpr::SpillRef(_)
        | TemplateExpr::Opaque { .. }
        | TemplateExpr::PreservedCached { .. }
        | TemplateExpr::MissingArg => Err(WireConvertError::UnsupportedInV1),
    }
}

fn template_expr_from_wire(
    w: WireTemplateExprV1,
    depth: usize,
) -> Result<TemplateExpr, WireConvertError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(WireConvertError::FormulaLimitExceeded);
    }
    let next = depth.saturating_add(1);
    match w {
        WireTemplateExprV1::Literal(lit) => Ok(TemplateExpr::Literal(lit.try_into()?)),
        WireTemplateExprV1::ArrayLiteral {
            rows,
            cols,
            elements,
        } => {
            if rows == 0 || cols == 0 {
                return Err(WireConvertError::InvalidArrayLiteral);
            }
            let expected = (rows as usize)
                .checked_mul(cols as usize)
                .ok_or(WireConvertError::InvalidArrayLiteral)?;
            if expected != elements.len() {
                return Err(WireConvertError::InvalidArrayLiteral);
            }
            let converted: Result<Vec<Literal>, WireConvertError> =
                elements.into_iter().map(Literal::try_from).collect();
            Ok(TemplateExpr::ArrayLiteral {
                rows,
                cols,
                elements: converted?,
            })
        }
        WireTemplateExprV1::Reference(r) => Ok(TemplateExpr::Reference(r.into())),
        WireTemplateExprV1::Name(n) => {
            check_text_len(&n)?;
            Ok(TemplateExpr::Name(n))
        }
        WireTemplateExprV1::Paren(inner) => Ok(TemplateExpr::Paren(Box::new(
            template_expr_from_wire(*inner, next)?,
        ))),
        WireTemplateExprV1::Unary { op, expr } => Ok(TemplateExpr::Unary {
            op: op.into(),
            expr: Box::new(template_expr_from_wire(*expr, next)?),
        }),
        WireTemplateExprV1::Binary { op, left, right } => Ok(TemplateExpr::Binary {
            op: op.into(),
            left: Box::new(template_expr_from_wire(*left, next)?),
            right: Box::new(template_expr_from_wire(*right, next)?),
        }),
        WireTemplateExprV1::Call { name, args } => {
            if args.len() > MAX_FUNCTION_ARGS {
                return Err(WireConvertError::FormulaLimitExceeded);
            }
            check_text_len(&name)?;
            let mut out = Vec::with_capacity(args.len());
            for arg in args {
                out.push(template_expr_from_wire(arg, next)?);
            }
            Ok(TemplateExpr::Call {
                name: name.to_ascii_uppercase(),
                args: out,
            })
        }
    }
}

impl TryFrom<&TemplateFormula> for WireTemplateFormulaV1 {
    type Error = WireConvertError;

    fn try_from(t: &TemplateFormula) -> Result<Self, Self::Error> {
        Ok(Self {
            mode: t.mode.into(),
            root: template_expr_to_wire(&t.root)?,
        })
    }
}

impl TryFrom<WireTemplateFormulaV1> for TemplateFormula {
    type Error = WireConvertError;

    fn try_from(w: WireTemplateFormulaV1) -> Result<Self, Self::Error> {
        Ok(Self {
            mode: w.mode.into(),
            root: template_expr_from_wire(w.root, 0)?,
        })
    }
}

// ------------------------------------------------------------ CellInput & Op

impl TryFrom<&CellInput<BoundFormula>> for WireCellInputV1 {
    type Error = WireConvertError;

    fn try_from(inp: &CellInput<BoundFormula>) -> Result<Self, Self::Error> {
        match inp {
            CellInput::Empty => Ok(Self::Empty),
            CellInput::Scalar(s) => Ok(Self::Scalar(s.into())),
            CellInput::Formula { formula, cached } => Ok(Self::Formula {
                formula: WireBoundFormulaV1::try_from(formula)?,
                cached: cached.as_ref().map(WireScalarValueV1::from),
            }),
        }
    }
}

impl TryFrom<WireCellInputV1> for CellInput<BoundFormula> {
    type Error = WireConvertError;

    fn try_from(w: WireCellInputV1) -> Result<Self, Self::Error> {
        match w {
            WireCellInputV1::Empty => Ok(Self::Empty),
            WireCellInputV1::Scalar(s) => Ok(Self::Scalar(s.try_into()?)),
            WireCellInputV1::Formula { formula, cached } => Ok(Self::Formula {
                formula: formula.try_into()?,
                cached: match cached {
                    Some(c) => Some(c.try_into()?),
                    None => None,
                },
            }),
        }
    }
}

impl TryFrom<&CellEdit<BoundFormula>> for WireCellEditV1 {
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

impl TryFrom<WireCellEditV1> for CellEdit<BoundFormula> {
    type Error = WireConvertError;

    fn try_from(w: WireCellEditV1) -> Result<Self, Self::Error> {
        Ok(Self {
            row: RowId(w.row),
            col: ColId(w.col),
            input: w.input.try_into()?,
            format: w.format.try_into()?,
        })
    }
}

impl TryFrom<&Op<BoundFormula>> for WireOpV1 {
    type Error = WireConvertError;

    fn try_from(op: &Op<BoundFormula>) -> Result<Self, Self::Error> {
        let converted = match op {
            Op::SetCells { sheet, edits } => Self::SetCells {
                sheet: sheet.0,
                edits: edits
                    .iter()
                    .map(WireCellEditV1::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
            },
            Op::SetCellsIf { .. }
            | Op::PermuteContent { .. }
            | Op::MoveCells { .. }
            | Op::DefineNamedRange { .. }
            | Op::DeleteNamedRange { .. } => return Err(WireConvertError::UnsupportedInV1),
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
                            .map(WireCellInputV1::try_from)
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
        };
        Ok(converted)
    }
}

impl TryFrom<WireOpV1> for Op<BoundFormula> {
    type Error = WireConvertError;

    fn try_from(w: WireOpV1) -> Result<Self, Self::Error> {
        match w {
            WireOpV1::SetCells { sheet, edits } => {
                let converted: Result<Vec<CellEdit<BoundFormula>>, WireConvertError> =
                    edits.into_iter().map(CellEdit::try_from).collect();
                Ok(Self::SetCells {
                    sheet: SheetId(sheet),
                    edits: converted?,
                })
            }
            WireOpV1::ClearRange { sheet, rows, cols } => Ok(Self::ClearRange {
                sheet: SheetId(sheet),
                rows: runs_from_wire(rows)?,
                cols: runs_from_wire(cols)?,
            }),
            WireOpV1::InsertRows { sheet, after, ids } => Ok(Self::InsertRows {
                sheet: SheetId(sheet),
                after: after.map(RowId),
                ids: ids.try_into()?,
            }),
            WireOpV1::InsertCols { sheet, after, ids } => Ok(Self::InsertCols {
                sheet: SheetId(sheet),
                after: after.map(ColId),
                ids: ids.try_into()?,
            }),
            WireOpV1::DeleteRows { sheet, ids } => Ok(Self::DeleteRows {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV1::DeleteCols { sheet, ids } => Ok(Self::DeleteCols {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV1::UndeleteRows { sheet, ids } => Ok(Self::UndeleteRows {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV1::UndeleteCols { sheet, ids } => Ok(Self::UndeleteCols {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
            }),
            WireOpV1::MoveRows { sheet, ids, after } => Ok(Self::MoveRows {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
                after: after.map(RowId),
            }),
            WireOpV1::MoveCols { sheet, ids, after } => Ok(Self::MoveCols {
                sheet: SheetId(sheet),
                ids: runs_from_wire(ids)?,
                after: after.map(ColId),
            }),
            WireOpV1::ResizeRows { sheet, sizes } => {
                let mut out = Vec::with_capacity(sizes.len());
                for (r, px) in sizes {
                    out.push((r.try_into()?, px));
                }
                Ok(Self::ResizeRows {
                    sheet: SheetId(sheet),
                    sizes: out,
                })
            }
            WireOpV1::ResizeCols { sheet, sizes } => {
                let mut out = Vec::with_capacity(sizes.len());
                for (c, px) in sizes {
                    out.push((c.try_into()?, px));
                }
                Ok(Self::ResizeCols {
                    sheet: SheetId(sheet),
                    sizes: out,
                })
            }
            WireOpV1::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            } => Ok(Self::SetFreeze {
                sheet: SheetId(sheet),
                frozen_rows,
                frozen_cols,
            }),
            WireOpV1::SetFormat {
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
            WireOpV1::SetBlock {
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
            WireOpV1::FillFormula {
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
            WireOpV1::AddSheet { sheet, name, after } => {
                check_text_len(&name)?;
                Ok(Self::AddSheet {
                    sheet: SheetId(sheet),
                    name,
                    after: after.map(SheetId),
                })
            }
            WireOpV1::RenameSheet { sheet, name } => {
                check_text_len(&name)?;
                Ok(Self::RenameSheet {
                    sheet: SheetId(sheet),
                    name,
                })
            }
            WireOpV1::DeleteSheet { sheet } => Ok(Self::DeleteSheet {
                sheet: SheetId(sheet),
            }),
            WireOpV1::UndeleteSheet { sheet } => Ok(Self::UndeleteSheet {
                sheet: SheetId(sheet),
            }),
            WireOpV1::MoveSheet { sheet, after } => Ok(Self::MoveSheet {
                sheet: SheetId(sheet),
                after: after.map(SheetId),
            }),
        }
    }
}

/// Converts a slice of in-memory operations into a [`WireTxnV1`].
pub fn ops_to_wire_txn(
    label: alloc::string::String,
    ops: &[Op<BoundFormula>],
) -> Result<WireTxnV1, WireConvertError> {
    let converted: Result<Vec<WireOpV1>, WireConvertError> =
        ops.iter().map(WireOpV1::try_from).collect();
    Ok(WireTxnV1 {
        label,
        ops: converted?,
    })
}

/// Validates and converts a [`WireTxnV1`] back into `(label, Vec<Op<BoundFormula>>)`.
pub fn ops_from_wire_txn(
    w: WireTxnV1,
) -> Result<(alloc::string::String, Vec<Op<BoundFormula>>), WireConvertError> {
    check_text_len(&w.label)?;
    let ops: Result<Vec<Op<BoundFormula>>, WireConvertError> =
        w.ops.into_iter().map(Op::try_from).collect();
    Ok((w.label, ops?))
}
