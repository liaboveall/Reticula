//! WebAssembly bindings: the entry point of the engine worker (`13-m1a.md` 6.1
//! & `14-m1b.md` 5–7).
//!
//! The engine is driven like a state machine. A command goes in through
//! [`Engine::handle`], which may answer with an event; viewport pages the
//! command made due are then taken out one at a time with
//! [`Engine::take_page`], and WAL frames queued by mutating transactions are
//! drained via [`Engine::take_pending_wal_frame`].
//!
//! Everything that crosses the worker boundary as a structured object is
//! defined here and exported to TypeScript via `tsify`, so there is exactly one
//! definition of each type.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

use reticula_core::csv::{CsvError, CsvReader, export_sheet_delimited};
use reticula_core::format::parse_format_code;
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::ops::{CellInput, FormatPatch, FormatTarget, InverseOp, Op, OpError, Workbook};
use reticula_core::sheet::Sheet;
use reticula_core::style::{
    BorderSet, BorderWeight, CellStyle, ColorPatch, ColorValue, HorizontalAlign, StylePatch,
};
use reticula_core::value::Value;
use reticula_core::viewport::{self, PAGE_COLS, PAGE_ROWS, Page, PageRange};
use reticula_formula::address::CellAddr;
use reticula_formula::binder::BoundFormula;
use reticula_formula::determinism;
use reticula_formula::engine::{EditError, FormulaEngine, RecalcStatus};
use reticula_formula::lang_service::{
    self, CompletionItem, CompletionKind, FormulaAnalysis, FormulaDiagnostic, FormulaRefHighlight,
    SignatureHelp, SyntaxTokenKind, SyntaxTokenSpan,
};
use reticula_formula::parser::ParseError;
use reticula_wire::convert_v3::ops_from_wire_txn_v3;
use reticula_wire::snapshot::{
    SnapshotEncoder, SnapshotStep, decode_snapshot_any, encode_seed_stub_v1, encode_snapshot_v1,
};
use reticula_wire::v3::{WireOpV3, WireTxnV3, WireWalRecordV3};
use reticula_wire::wal::{
    WalCorruptionV3, WalTailDisposition, encode_wal_init_v3, encode_wal_record_v3,
    replay_wal_records_v3, scan_wal_any_v3,
};
use serde::{Deserialize, Serialize};
use tsify::{Ts, Tsify};
use wasm_bindgen::prelude::*;

#[cfg(feature = "bench")]
pub mod bench;
#[cfg(feature = "bench")]
pub mod bench_sync;
pub mod clipboard;
pub mod collab;
pub mod history;
pub mod undo;

use crate::clipboard::{
    InternalClipboard, RectPos, build_fill_ops, build_paste_ops, copy_range, find_autofill_end_row,
};
use crate::undo::{UndoStack, collab_compensations};
use reticula_core::ops::{FormulaMode, NamedRangeKey, is_valid_named_range_name};
use reticula_formula::filter::{ColFilterCriterion, FilterCmpOp};
use reticula_formula::search::{self, FindOptions, SearchIn, SearchScope};
use reticula_formula::sort::{self, SortKey};

/// Maximum raw CSV/TSV file size accepted for import (`256 MiB`, `14-m1b.md` 5.6).
pub const MAX_CSV_IMPORT_BYTES: usize = 256 * 1024 * 1024;

/// Time budget per recalculation slice in milliseconds (`13-m1a.md` 5.6).
const RECALC_SLICE_BUDGET_MS: f64 = 4.0;
/// Formula count evaluated between monotonic clock checks inside a slice.
const RECALC_CHECK_INTERVAL: usize = 64;
/// Above this many dirty cell notifications in a single wave, re-enqueue all
/// currently subscribed pages directly instead of mapping each cell to a page.
const BULK_DIRTY_PAGE_THRESHOLD: usize = 2_048;

static ENGINE_PANICKED: AtomicBool = AtomicBool::new(false);

mod wire_u64 {
    use serde::de::{self, Visitor};
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        struct U64Visitor;

        impl Visitor<'_> for U64Visitor {
            type Value = u64;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a 64-bit unsigned integer as a decimal string or non-negative integer")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse::<u64>().map_err(E::custom)
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(v)
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(U64Visitor)
    }
}

mod wire_opt_u64 {
    use serde::{Deserialize, Deserializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawOpt {
        Text(String),
        Num(u64),
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        let opt = Option::<RawOpt>::deserialize(deserializer)?;
        match opt {
            None => Ok(None),
            Some(RawOpt::Num(n)) => Ok(Some(n)),
            Some(RawOpt::Text(s)) => s.parse::<u64>().map(Some).map_err(serde::de::Error::custom),
        }
    }
}

// ============================================================ Wire DTOs (tsify)

/// Contiguous run of row or column identifiers (`firstId .. firstId + len`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireIdRun {
    /// First 64-bit identifier encoded as an exact decimal string.
    #[serde(with = "wire_u64")]
    #[tsify(type = "string")]
    pub first_id: u64,
    /// Number of consecutive identifiers (`>= 1`).
    pub len: u32,
}

impl From<WireIdRun> for IdRun {
    fn from(w: WireIdRun) -> Self {
        Self {
            first_id: w.first_id,
            len: w.len,
        }
    }
}

/// Row or column run paired with an explicit pixel size (`null` resets to default).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireAxisSize {
    /// Target row/column run.
    pub run: WireIdRun,
    /// Custom height/width in CSS pixels, or `null` to restore default.
    pub size_px: Option<u16>,
}

/// Worksheet tab descriptor in tab-bar order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct SheetTabInfo {
    /// Stable worksheet identifier.
    pub id: u32,
    /// Display name (`1..=31` UTF-16 code units).
    pub name: String,
}

/// Theme-adaptive palette slot or explicit 24-bit RGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WireColorValue {
    /// Semantic light/dark-adaptive palette slot (`0..7`).
    Palette { slot: u8 },
    /// 24-bit sRGB (`0x00RRGGBB`).
    Rgb { rgb: u32 },
}

impl From<ColorValue> for WireColorValue {
    fn from(c: ColorValue) -> Self {
        match c {
            ColorValue::Palette(slot) => Self::Palette { slot },
            ColorValue::Rgb(rgb) => Self::Rgb { rgb },
        }
    }
}

impl From<WireColorValue> for ColorValue {
    fn from(w: WireColorValue) -> Self {
        match w {
            WireColorValue::Palette { slot } => Self::Palette(slot),
            WireColorValue::Rgb { rgb } => Self::Rgb(rgb),
        }
    }
}

/// Edge border stroke weight.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum WireBorderWeight {
    #[default]
    None,
    Thin,
    Medium,
}

impl From<BorderWeight> for WireBorderWeight {
    fn from(b: BorderWeight) -> Self {
        match b {
            BorderWeight::None => Self::None,
            BorderWeight::Thin => Self::Thin,
            BorderWeight::Medium => Self::Medium,
        }
    }
}

impl From<WireBorderWeight> for BorderWeight {
    fn from(w: WireBorderWeight) -> Self {
        match w {
            WireBorderWeight::None => Self::None,
            WireBorderWeight::Thin => Self::Thin,
            WireBorderWeight::Medium => Self::Medium,
        }
    }
}

/// Horizontal cell alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum WireHorizontalAlign {
    #[default]
    Auto,
    Left,
    Center,
    Right,
}

impl From<HorizontalAlign> for WireHorizontalAlign {
    fn from(a: HorizontalAlign) -> Self {
        match a {
            HorizontalAlign::Auto => Self::Auto,
            HorizontalAlign::Left => Self::Left,
            HorizontalAlign::Center => Self::Center,
            HorizontalAlign::Right => Self::Right,
        }
    }
}

impl From<WireHorizontalAlign> for HorizontalAlign {
    fn from(w: WireHorizontalAlign) -> Self {
        match w {
            WireHorizontalAlign::Auto => Self::Auto,
            WireHorizontalAlign::Left => Self::Left,
            WireHorizontalAlign::Center => Self::Center,
            WireHorizontalAlign::Right => Self::Right,
        }
    }
}

/// Resolved cell visual style indexed by `styleId` in viewport v2 pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireCellStyle {
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub align: WireHorizontalAlign,
    pub text_color: Option<WireColorValue>,
    pub fill_color: Option<WireColorValue>,
    pub border_top: WireBorderWeight,
    pub border_right: WireBorderWeight,
    pub border_bottom: WireBorderWeight,
    pub border_left: WireBorderWeight,
}

impl From<CellStyle> for WireCellStyle {
    fn from(s: CellStyle) -> Self {
        Self {
            bold: s.bold,
            italic: s.italic,
            strikethrough: s.strikethrough,
            align: s.align.into(),
            text_color: s.text_color.map(Into::into),
            fill_color: s.fill_color.map(Into::into),
            border_top: s.borders.top.into(),
            border_right: s.borders.right.into(),
            border_bottom: s.borders.bottom.into(),
            border_left: s.borders.left.into(),
        }
    }
}

impl From<WireCellStyle> for CellStyle {
    fn from(w: WireCellStyle) -> Self {
        Self {
            bold: w.bold,
            italic: w.italic,
            strikethrough: w.strikethrough,
            align: w.align.into(),
            text_color: w.text_color.map(Into::into),
            fill_color: w.fill_color.map(Into::into),
            borders: BorderSet {
                top: w.border_top.into(),
                right: w.border_right.into(),
                bottom: w.border_bottom.into(),
                left: w.border_left.into(),
            },
        }
    }
}

/// Tri-state color override in [`WireStylePatch`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WireColorPatch {
    #[default]
    Keep,
    Clear,
    Set {
        color: WireColorValue,
    },
}

impl From<WireColorPatch> for ColorPatch {
    fn from(w: WireColorPatch) -> Self {
        match w {
            WireColorPatch::Keep => Self::Keep,
            WireColorPatch::Clear => Self::Clear,
            WireColorPatch::Set { color } => Self::Set(color.into()),
        }
    }
}

/// Partial style override sent in [`EngineCommand::SetFormat`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireStylePatch {
    #[serde(default)]
    pub bold: Option<bool>,
    #[serde(default)]
    pub italic: Option<bool>,
    #[serde(default)]
    pub strikethrough: Option<bool>,
    #[serde(default)]
    pub align: Option<WireHorizontalAlign>,
    #[serde(default)]
    pub text_color: WireColorPatch,
    #[serde(default)]
    pub fill_color: WireColorPatch,
    #[serde(default)]
    pub border_top: Option<WireBorderWeight>,
    #[serde(default)]
    pub border_right: Option<WireBorderWeight>,
    #[serde(default)]
    pub border_bottom: Option<WireBorderWeight>,
    #[serde(default)]
    pub border_left: Option<WireBorderWeight>,
}

impl From<WireStylePatch> for StylePatch {
    fn from(w: WireStylePatch) -> Self {
        Self {
            bold: w.bold,
            italic: w.italic,
            strikethrough: w.strikethrough,
            align: w.align.map(Into::into),
            text_color: w.text_color.into(),
            fill_color: w.fill_color.into(),
            border_top: w.border_top.map(Into::into),
            border_right: w.border_right.map(Into::into),
            border_bottom: w.border_bottom.map(Into::into),
            border_left: w.border_left.map(Into::into),
        }
    }
}

/// Target scope of [`EngineCommand::SetFormat`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(
    tag = "scope",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WireFormatTarget {
    Cells {
        rows: Vec<WireIdRun>,
        cols: Vec<WireIdRun>,
    },
    Rows {
        rows: Vec<WireIdRun>,
    },
    Cols {
        cols: Vec<WireIdRun>,
    },
}

impl From<WireFormatTarget> for FormatTarget {
    fn from(w: WireFormatTarget) -> Self {
        match w {
            WireFormatTarget::Cells { rows, cols } => Self::Cells {
                rows: rows.into_iter().map(IdRun::from).collect(),
                cols: cols.into_iter().map(IdRun::from).collect(),
            },
            WireFormatTarget::Rows { rows } => {
                Self::Rows(rows.into_iter().map(IdRun::from).collect())
            }
            WireFormatTarget::Cols { cols } => {
                Self::Cols(cols.into_iter().map(IdRun::from).collect())
            }
        }
    }
}

/// Number-format override sent in [`EngineCommand::SetFormat`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WireFormatPatch {
    #[default]
    Keep,
    Clear,
    /// Standard format code (`"General"`, `"0.00"`, `"#,##0.00"`, `"$#,##0.00"`,
    /// `"¥#,##0.00"`, `"0.00%"`, `"yyyy-mm-dd"`, `"@"`, ...).
    Set {
        code: String,
    },
}

/// Cardinal direction for `Ctrl+Arrow` / `Ctrl+Shift+Arrow` edge jumping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum JumpDirection {
    Up,
    Down,
    Left,
    Right,
}

// Language service tsify wrappers (kept 1-to-1 with `reticula_formula::lang_service`)

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum WireSyntaxTokenKind {
    Equals,
    Function,
    Reference,
    SheetRef,
    Number,
    String,
    Boolean,
    Error,
    Operator,
    Paren,
    Punctuation,
    Unknown,
}

impl From<SyntaxTokenKind> for WireSyntaxTokenKind {
    fn from(k: SyntaxTokenKind) -> Self {
        match k {
            SyntaxTokenKind::Equals => Self::Equals,
            SyntaxTokenKind::Function => Self::Function,
            SyntaxTokenKind::Reference => Self::Reference,
            SyntaxTokenKind::SheetRef => Self::SheetRef,
            SyntaxTokenKind::Number => Self::Number,
            SyntaxTokenKind::String => Self::String,
            SyntaxTokenKind::Boolean => Self::Boolean,
            SyntaxTokenKind::Error => Self::Error,
            SyntaxTokenKind::Operator => Self::Operator,
            SyntaxTokenKind::Paren => Self::Paren,
            SyntaxTokenKind::Punctuation => Self::Punctuation,
            SyntaxTokenKind::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireSyntaxTokenSpan {
    pub start_utf16: u32,
    pub end_utf16: u32,
    pub kind: WireSyntaxTokenKind,
    pub color_index: Option<u8>,
    pub matched_bracket: bool,
}

impl From<SyntaxTokenSpan> for WireSyntaxTokenSpan {
    fn from(s: SyntaxTokenSpan) -> Self {
        Self {
            start_utf16: s.start_utf16,
            end_utf16: s.end_utf16,
            kind: s.kind.into(),
            color_index: s.color_index,
            matched_bracket: s.matched_bracket,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireFormulaDiagnostic {
    pub start_utf16: u32,
    pub end_utf16: u32,
    pub message: String,
}

impl From<FormulaDiagnostic> for WireFormulaDiagnostic {
    fn from(d: FormulaDiagnostic) -> Self {
        Self {
            start_utf16: d.start_utf16,
            end_utf16: d.end_utf16,
            message: d.message,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum WireCompletionKind {
    Function,
    Sheet,
    NamedRange,
}

impl From<CompletionKind> for WireCompletionKind {
    fn from(k: CompletionKind) -> Self {
        match k {
            CompletionKind::Function => Self::Function,
            CompletionKind::Sheet => Self::Sheet,
            CompletionKind::NamedRange => Self::NamedRange,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireCompletionItem {
    pub label: String,
    pub insert_text: String,
    pub replace_start_utf16: u32,
    pub replace_end_utf16: u32,
    pub kind: WireCompletionKind,
    pub signature: String,
    pub summary: String,
}

impl From<CompletionItem> for WireCompletionItem {
    fn from(c: CompletionItem) -> Self {
        Self {
            label: c.label,
            insert_text: c.insert_text,
            replace_start_utf16: c.replace_start_utf16,
            replace_end_utf16: c.replace_end_utf16,
            kind: c.kind.into(),
            signature: c.signature,
            summary: c.summary,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireSignatureHelp {
    pub name: String,
    pub params: Vec<String>,
    pub active_param: u32,
    pub summary: String,
}

impl From<SignatureHelp> for WireSignatureHelp {
    fn from(s: SignatureHelp) -> Self {
        Self {
            name: s.name,
            params: s.params,
            active_param: s.active_param,
            summary: s.summary,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireFormulaRefHighlight {
    pub start_utf16: u32,
    pub end_utf16: u32,
    pub color_index: u8,
    pub sheet: u32,
    pub first_row: u32,
    pub last_row: u32,
    pub first_col: u32,
    pub last_col: u32,
}

impl From<FormulaRefHighlight> for WireFormulaRefHighlight {
    fn from(r: FormulaRefHighlight) -> Self {
        Self {
            start_utf16: r.start_utf16,
            end_utf16: r.end_utf16,
            color_index: r.color_index,
            sheet: r.sheet,
            first_row: r.first_row,
            last_row: r.last_row,
            first_col: r.first_col,
            last_col: r.last_col,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireFormulaAnalysis {
    pub is_formula: bool,
    pub tokens: Vec<WireSyntaxTokenSpan>,
    pub diagnostics: Vec<WireFormulaDiagnostic>,
    pub completions: Vec<WireCompletionItem>,
    pub signature: Option<WireSignatureHelp>,
    pub references: Vec<WireFormulaRefHighlight>,
    pub can_insert_ref: bool,
    pub active_ref_span: Option<(u32, u32)>,
}

impl From<FormulaAnalysis> for WireFormulaAnalysis {
    fn from(a: FormulaAnalysis) -> Self {
        Self {
            is_formula: a.is_formula,
            tokens: a.tokens.into_iter().map(Into::into).collect(),
            diagnostics: a.diagnostics.into_iter().map(Into::into).collect(),
            completions: a.completions.into_iter().map(Into::into).collect(),
            signature: a.signature.map(Into::into),
            references: a.references.into_iter().map(Into::into).collect(),
            can_insert_ref: a.can_insert_ref,
            active_ref_span: a.active_ref_span,
        }
    }
}

// Step 4 DTOs: AutoFilter, Sort, Find & Replace, Named Ranges, and Dynamic Spills

/// 0-based inclusive cell rectangle in visible display coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireCellRect {
    pub first_row: u32,
    pub last_row: u32,
    pub first_col: u32,
    pub last_col: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum WireFilterCmpOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl From<WireFilterCmpOp> for FilterCmpOp {
    fn from(op: WireFilterCmpOp) -> Self {
        match op {
            WireFilterCmpOp::Eq => Self::Eq,
            WireFilterCmpOp::Ne => Self::Ne,
            WireFilterCmpOp::Gt => Self::Gt,
            WireFilterCmpOp::Ge => Self::Ge,
            WireFilterCmpOp::Lt => Self::Lt,
            WireFilterCmpOp::Le => Self::Le,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Tsify)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WireColFilterCriterion {
    Values {
        allowed: Vec<String>,
        include_blanks: bool,
    },
    TextContains {
        needle: String,
        case_sensitive: bool,
    },
    TextStartsWith {
        prefix: String,
        case_sensitive: bool,
    },
    TextEndsWith {
        suffix: String,
        case_sensitive: bool,
    },
    NumberCompare {
        op: WireFilterCmpOp,
        value: f64,
    },
    NumberBetween {
        min: f64,
        max: f64,
    },
    TopN {
        count: u32,
        top: bool,
    },
}

impl From<WireColFilterCriterion> for ColFilterCriterion {
    fn from(c: WireColFilterCriterion) -> Self {
        match c {
            WireColFilterCriterion::Values {
                allowed,
                include_blanks,
            } => Self::Values {
                allowed: allowed.into_iter().collect(),
                include_blanks,
            },
            WireColFilterCriterion::TextContains {
                needle,
                case_sensitive,
            } => Self::TextContains {
                needle,
                case_sensitive,
            },
            WireColFilterCriterion::TextStartsWith {
                prefix,
                case_sensitive,
            } => Self::TextStartsWith {
                prefix,
                case_sensitive,
            },
            WireColFilterCriterion::TextEndsWith {
                suffix,
                case_sensitive,
            } => Self::TextEndsWith {
                suffix,
                case_sensitive,
            },
            WireColFilterCriterion::NumberCompare { op, value } => Self::NumberCompare {
                op: op.into(),
                value,
            },
            WireColFilterCriterion::NumberBetween { min, max } => Self::NumberBetween { min, max },
            WireColFilterCriterion::TopN { count, top } => Self::TopN { count, top },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireFilterState {
    pub header_row: u32,
    pub last_row: u32,
    pub first_col: u32,
    pub last_col: u32,
    pub active_cols: Vec<u32>,
    pub hidden_row_spans: Vec<(u32, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireDistinctValue {
    pub value: String,
    pub count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireSortKey {
    pub col_offset: u32,
    pub ascending: bool,
    pub case_sensitive: bool,
}

impl From<WireSortKey> for SortKey {
    fn from(k: WireSortKey) -> Self {
        Self {
            col_offset: k.col_offset,
            ascending: k.ascending,
            case_sensitive: k.case_sensitive,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum WireSearchScope {
    ActiveSheet,
    WholeWorkbook,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum WireSearchIn {
    DisplayValues,
    Formulas,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireFindOptions {
    pub query: String,
    pub scope: WireSearchScope,
    pub search_in: WireSearchIn,
    pub case_sensitive: bool,
    pub whole_cell: bool,
    pub use_regex: bool,
    pub skip_hidden: bool,
}

impl From<WireFindOptions> for FindOptions {
    fn from(o: WireFindOptions) -> Self {
        Self {
            query: o.query,
            scope: match o.scope {
                WireSearchScope::ActiveSheet => SearchScope::ActiveSheet,
                WireSearchScope::WholeWorkbook => SearchScope::WholeWorkbook,
            },
            search_in: match o.search_in {
                WireSearchIn::DisplayValues => SearchIn::DisplayValues,
                WireSearchIn::Formulas => SearchIn::Formulas,
            },
            case_sensitive: o.case_sensitive,
            whole_cell: o.whole_cell,
            use_regex: o.use_regex,
            skip_hidden: o.skip_hidden,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireSearchMatch {
    pub sheet: u32,
    pub sheet_name: String,
    pub row: u32,
    pub physical_row: u32,
    pub col: u32,
    #[serde(with = "wire_u64")]
    #[tsify(type = "string")]
    pub row_id: u64,
    #[serde(with = "wire_u64")]
    #[tsify(type = "string")]
    pub col_id: u64,
    pub text: String,
    pub is_formula: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireNamedRangeInfo {
    pub scope: Option<u32>,
    pub name: String,
    pub formula: String,
    pub comment: Option<String>,
    pub hidden: bool,
}

// Persistence & Safe Mode DTOs

/// Metadata returned by one time-sliced [`Engine::step_snapshot_stream`] call;
/// binary chunk bytes are retrieved zero-copy via [`Engine::take_snapshot_chunk`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Tsify)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WireSnapshotStep {
    /// Internal scan step completed; yield or continue.
    Progress,
    /// Write the pending byte chunk (`takeSnapshotChunk()`) at `offset`.
    Chunk { offset: f64, byte_len: u32 },
    /// Write the pending header chunk (`takeSnapshotChunk()`) at `offset = 0`,
    /// truncate file to `total_len`, and `flush()`.
    Finished { total_len: f64 },
}

/// Summary of one tail WAL transaction shown in D-029 Safe Mode (`14-m1b.md` 7.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireWalTxnSummary {
    #[serde(with = "wire_u64")]
    #[tsify(type = "string")]
    pub seq: u64,
    pub timestamp_ms: f64,
    pub label: String,
}

/// Result of inspecting a WAL file for corruption / Safe Mode recovery options.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Tsify)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WireWalInspection {
    /// Clean or recoverable crash tail (`truncate_to_bytes` is `Some(n)` when a
    /// torn final record or zero-padded tail should be truncated).
    Valid {
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        base_seq: u64,
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        last_seq: u64,
        record_count: u32,
        truncate_to_bytes: Option<u32>,
        tail_txns: Vec<WireWalTxnSummary>,
    },
    /// Unrecoverable mid-file or header corruption; enter D-029 Safe Mode.
    Corrupted {
        reason: String,
        salvageable_count: u32,
        salvageable_tail_txns: Vec<WireWalTxnSummary>,
    },
}

/// Outcome of loading a snapshot + WAL into [`Engine`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireLoadOutcome {
    pub info: EngineInfo,
    pub sheet: SheetInfo,
    pub replayed_txns: u32,
    pub truncate_wal_to_bytes: Option<u32>,
    pub recalc_frozen: bool,
}

/// Outcome of one 65,536-row step during [`Engine::step_csv_import`].
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Tsify)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WireCsvImportStep {
    Progress { rows_parsed: u32, percent: u8 },
    Done { info: EngineInfo, sheet: SheetInfo },
}

// ================================================== EngineCommand & EngineEvent

/// A message from the main thread to the engine worker (`13-m1a.md` 6.1 & `14-m1b.md` 6–7).
#[derive(Debug, Deserialize, Tsify)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EngineCommand {
    /// Replace the document with the seeded 1M-row sample (including parameter
    /// panel `U1:V2` and 100,000 bound `M`-column formulas). Answered by
    /// [`EngineEvent::Ready`].
    OpenSample {
        /// Seed of the sample. The same seed gives the same sheet everywhere.
        seed: u32,
        /// Number of rows, the header row included.
        rows: u32,
    },
    /// Replace the document with a fresh empty standard workbook (`Sheet1`).
    OpenEmpty,
    /// Declare the range of cells the grid wants to hold: what it shows plus a
    /// margin around it (pinned frozen-pane pages are retained automatically).
    Subscribe {
        /// Target sheet identifier (`0` for the primary sheet).
        #[serde(default)]
        sheet: u32,
        /// A number the grid picks; every page sent for this subscription
        /// carries it.
        epoch: u32,
        /// Position of the first row of the range.
        first_row: u32,
        /// Number of rows in the range.
        row_count: u32,
        /// Position of the first column of the range.
        first_col: u32,
        /// Number of columns in the range.
        col_count: u32,
    },
    /// Switch active worksheet tab and return its [`SheetInfo`].
    SwitchSheet { sheet: u32 },
    /// Inject pure calendar/randomness state prior to volatile recalculation (`14-m1b.md` 5.3).
    SetEvalEnv {
        now_serial: f64,
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        seed: u64,
        #[serde(default)]
        timestamp_ms: Option<f64>,
    },
    /// Query the raw input / formula text, formatting and style of a single cell.
    InspectCell {
        sheet: u32,
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        row_id: u64,
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        col_id: u64,
    },
    /// Submit raw user input (`raw`) to `(sheet, row_id, col_id)` under E30.
    EditCell {
        sheet: u32,
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        row_id: u64,
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        col_id: u64,
        raw: String,
    },
    /// Clear values and formulas in `rows × cols` (preserving styles/formats).
    ClearCells {
        sheet: u32,
        rows: Vec<WireIdRun>,
        cols: Vec<WireIdRun>,
    },
    /// Insert `count` new rows after `after_row_id` (`null` inserts before row 1).
    InsertRows {
        sheet: u32,
        #[serde(deserialize_with = "wire_opt_u64::deserialize")]
        #[tsify(type = "string | null")]
        after_row_id: Option<u64>,
        count: u32,
    },
    /// Delete the specified row ID runs (tombstoning them on the row axis).
    DeleteRows { sheet: u32, rows: Vec<WireIdRun> },
    /// Insert `count` new columns after `after_col_id` (`null` inserts before col A).
    InsertCols {
        sheet: u32,
        #[serde(deserialize_with = "wire_opt_u64::deserialize")]
        #[tsify(type = "string | null")]
        after_col_id: Option<u64>,
        count: u32,
    },
    /// Delete the specified column ID runs.
    DeleteCols { sheet: u32, cols: Vec<WireIdRun> },
    /// Reorder `rows` to sit after `after_row_id` (`null` moves to top, with E25 handoff).
    MoveRows {
        sheet: u32,
        rows: Vec<WireIdRun>,
        #[serde(deserialize_with = "wire_opt_u64::deserialize")]
        #[tsify(type = "string | null")]
        after_row_id: Option<u64>,
    },
    /// Reorder `cols` to sit after `after_col_id` (`null` moves to left, with E25 handoff).
    MoveCols {
        sheet: u32,
        cols: Vec<WireIdRun>,
        #[serde(deserialize_with = "wire_opt_u64::deserialize")]
        #[tsify(type = "string | null")]
        after_col_id: Option<u64>,
    },
    /// Set or reset custom pixel heights on row runs.
    ResizeRows {
        sheet: u32,
        sizes: Vec<WireAxisSize>,
    },
    /// Set or reset custom pixel widths on column runs.
    ResizeCols {
        sheet: u32,
        sizes: Vec<WireAxisSize>,
    },
    /// Set frozen top row count and left column count (`0..=32`).
    SetFreeze {
        sheet: u32,
        frozen_rows: u32,
        frozen_cols: u32,
    },
    /// Apply number format and/or composable visual style patch to cells, rows, or columns.
    SetFormat {
        sheet: u32,
        target: WireFormatTarget,
        #[serde(default)]
        number_format: WireFormatPatch,
        #[serde(default)]
        style: WireStylePatch,
    },
    /// Drag-fill from `src` across union rectangle `dst` (using constant-size
    /// `Op::FillFormula` for formulas or series extrapolation for values).
    FillRange {
        sheet: u32,
        src_first_row: u32,
        src_row_count: u32,
        src_first_col: u32,
        src_col_count: u32,
        dst_first_row: u32,
        dst_row_count: u32,
        dst_first_col: u32,
        dst_col_count: u32,
    },
    /// Double-click fill handle: auto-fill downward to match adjacent column extent.
    AutoFillDown {
        sheet: u32,
        src_first_row: u32,
        src_row_count: u32,
        src_first_col: u32,
        src_col_count: u32,
    },
    /// Capture selection as TSV + HTML `<table>` and record internal clipboard state.
    CopyRange {
        sheet: u32,
        first_row: u32,
        row_count: u32,
        first_col: u32,
        col_count: u32,
        cut: bool,
    },
    /// Paste internal or external (`text/html` / `text/plain`) clipboard content.
    PasteClipboard {
        sheet: u32,
        dest_row: u32,
        #[serde(default = "default_one")]
        dest_row_count: u32,
        dest_col: u32,
        #[serde(default = "default_one")]
        dest_col_count: u32,
        tsv: String,
        #[serde(default)]
        html: Option<String>,
    },
    /// Create a new worksheet tab (`name` defaults to `"SheetN"`, E28 disambiguated).
    AddSheet {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        after_sheet: Option<u32>,
    },
    /// Rename an existing worksheet.
    RenameSheet { sheet: u32, name: String },
    /// Tombstone a worksheet (rejected if last visible worksheet).
    DeleteSheet { sheet: u32 },
    /// Reorder worksheet tab to sit after `after_sheet` (`null` moves to first tab).
    MoveSheet {
        sheet: u32,
        after_sheet: Option<u32>,
    },
    /// Revert the most recent user transaction (`Ctrl+Z`).
    Undo,
    /// Re-apply the most recently undone user transaction (`Ctrl+Y` / `Ctrl+Shift+Z`).
    Redo,
    /// Formula-bar language service query (`< 0.2 ms`).
    AnalyzeFormula {
        sheet: u32,
        source: String,
        cursor_utf16: u32,
    },
    /// Cycle `$` absolute/relative locks at `cursor_utf16` (`F4`).
    ToggleFormulaDollar { source: String, cursor_utf16: u32 },
    /// Format a point-and-click cell/range selection for insertion into a formula.
    FormatRangeRef {
        host_sheet: u32,
        target_sheet: u32,
        r1: u32,
        c1: u32,
        r2: u32,
        c2: u32,
    },
    /// Excel-style `Ctrl+Arrow` / `Ctrl+Shift+Arrow` data-region boundary jump.
    JumpEdge {
        sheet: u32,
        row: u32,
        col: u32,
        dir: JumpDirection,
        select: bool,
    },
    /// Jump to bottom-rightmost non-empty cell on `sheet` (`Ctrl+End` / `Ctrl+Shift+End`).
    JumpUsedEnd { sheet: u32, select: bool },
    /// Unfreeze recalculation after opening a document in Safe Mode without recalc.
    ResumeRecalc,
    /// Advance an in-progress time-sliced recalculation by one ~4 ms budget slice.
    ContinueRecalc,
    /// Install or replace worker-local AutoFilter dropdowns on a rectangular table.
    SetFilterTable {
        sheet: u32,
        header_row: u32,
        last_row: u32,
        first_col: u32,
        last_col: u32,
    },
    /// Apply or clear one AutoFilter column criterion (`criterion = null` removes column filter).
    SetColumnFilter {
        sheet: u32,
        col: u32,
        criterion: Option<WireColFilterCriterion>,
    },
    /// Clear the active worker-local AutoFilter on `sheet`.
    ClearFilter { sheet: u32 },
    /// Collect sorted distinct values for an AutoFilter dropdown column.
    GetFilterValues { sheet: u32, col: u32 },
    /// Multi-key stable sort producing an atomic `Op::PermuteContent` transaction.
    SortRange {
        sheet: u32,
        first_row: u32,
        last_row: u32,
        first_col: u32,
        last_col: u32,
        has_header: Option<bool>,
        keys: Vec<WireSortKey>,
    },
    /// Bounded workbook/sheet search.
    FindAll {
        sheet: u32,
        options: WireFindOptions,
        max_results: Option<u32>,
    },
    /// Find next/previous matching cell relative to `(from_row, from_col)`.
    FindNext {
        sheet: u32,
        from_row: u32,
        from_col: u32,
        backwards: bool,
        options: WireFindOptions,
    },
    /// Atomic batch Replace All (`<= 10,000` edits, syntax-validated formulas).
    ReplaceAll {
        sheet: u32,
        options: WireFindOptions,
        replacement: String,
    },
    /// List all defined workbook- and worksheet-scoped named ranges.
    ListNamedRanges,
    /// Define or update a workbook- or worksheet-scoped named range (`Op::DefineNamedRange`).
    DefineNamedRange {
        scope: Option<u32>,
        name: String,
        formula: String,
        comment: Option<String>,
        hidden: Option<bool>,
    },
    /// Delete an existing named range (`Op::DeleteNamedRange`).
    DeleteNamedRange { scope: Option<u32>, name: String },
    /// Run the determinism self-check. Answered by [`EngineEvent::SelfChecked`].
    SelfCheck,
    /// Report the workbook's state digest (`STATE_DIGEST_VERSION`), which two
    /// replicas share exactly when they hold the same logical content.
    /// Answered by [`EngineEvent::StateDigested`].
    StateDigest,
}

const fn default_one() -> u32 {
    1
}

/// A message from the engine worker to the main thread (`13-m1a.md` 6.1 & `14-m1b.md` 6–7).
#[derive(Debug, PartialEq, Serialize, Tsify)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EngineEvent {
    /// A document is open and its cells can be subscribed to.
    Ready { info: EngineInfo, sheet: SheetInfo },
    /// Structural, sheet-tab, resize, freeze, format, fill, or undo/redo change completed.
    SheetChanged {
        sheet: SheetInfo,
        action_ms: f64,
        recalculated: u32,
    },
    /// Emitted when scrolling interns a new combined row×col style entry.
    StylesUpdated {
        sheet: u32,
        styles: Vec<WireCellStyle>,
    },
    /// Raw formula/value/format/style inspection details for a cell.
    CellInspected {
        sheet: u32,
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        row_id: u64,
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        col_id: u64,
        address: String,
        raw: String,
        display: String,
        is_formula: bool,
        error: Option<String>,
        format_code: String,
        style: WireCellStyle,
        spill_range: Option<WireCellRect>,
        is_spill_shadow: bool,
    },
    /// Language-service tokens, diagnostics, completions, signature help, and colored refs.
    FormulaAnalyzed { analysis: WireFormulaAnalysis },
    /// Result of `F4` `$`-anchor cycling.
    FormulaDollarToggled {
        changed: bool,
        source: String,
        start_utf16: u32,
        end_utf16: u32,
    },
    /// Formatted A1/cross-sheet reference string for point-and-click insertion.
    RangeRefFormatted { reference: String },
    /// Target cell coordinates answered for `JumpEdge` / `JumpUsedEnd`.
    EdgeJumped {
        sheet: u32,
        row: u32,
        col: u32,
        select: bool,
    },
    /// Generated TSV + HTML `<table>` payload ready to write to system clipboard.
    ClipboardCopied {
        sheet: u32,
        first_row: u32,
        row_count: u32,
        first_col: u32,
        col_count: u32,
        cut: bool,
        tsv: String,
        html: String,
        fingerprint: String,
    },
    /// Paste transaction applied (with pasted target bounds so grid selects it).
    ClipboardPasted {
        sheet: SheetInfo,
        dest_first_row: u32,
        dest_row_count: u32,
        dest_first_col: u32,
        dest_col_count: u32,
        recalculated: u32,
        elapsed_ms: f64,
    },
    /// Partial progress emitted when recalculation yields at a 4 ms slice boundary.
    RecalcProgress { done: u32, total: u32 },
    /// Incremental recalculation (or value edit) finished.
    RecalcDone {
        recalculated: u32,
        elapsed_ms: f64,
        sheet: SheetInfo,
    },
    /// Distinct values available for an AutoFilter column dropdown.
    FilterValues {
        sheet: u32,
        col: u32,
        values: Vec<WireDistinctValue>,
        blank_count: u32,
        truncated: bool,
    },
    /// Search match results for Find All.
    SearchResults {
        matches: Vec<WireSearchMatch>,
        total: u32,
        truncated: bool,
    },
    /// Result of Find Next / Find Previous navigation.
    SearchNavigated { found: Option<WireSearchMatch> },
    /// Batch Replace All result with refreshed worksheet metadata.
    ReplaceCompleted {
        replaced_cells: u32,
        skipped_invalid_formulas: u32,
        truncated: bool,
        sheet: SheetInfo,
    },
    /// Full defined named ranges listing.
    NamedRangesListed { items: Vec<WireNamedRangeInfo> },
    /// Command was atomically rejected without mutating workbook state.
    Rejected { reason: String },
    /// The determinism self-check has run.
    SelfChecked { digest: String },
    /// The workbook's state digest, as `v<version>:<content>:<order>` in hex.
    StateDigested { digest: String },
}

/// Facts about the running engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    /// Version of the engine kernel.
    pub kernel_version: String,
}

/// Facts about the active worksheet and workbook undo/WAL state (`14-m1b.md` 6.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct SheetInfo {
    /// Active sheet identifier (`0` for primary sheet).
    pub sheet: u32,
    /// Active worksheet name.
    pub sheet_name: String,
    /// Ordered list of live worksheet tabs.
    pub sheets: Vec<SheetTabInfo>,
    /// Number of visible rows on the active sheet (after worker-local filter projection).
    pub rows: u32,
    /// Total physical live rows on the active sheet.
    pub total_rows: u32,
    /// Number of live columns on the active sheet.
    pub cols: u32,
    /// Active worker-local AutoFilter bounds, filtered columns, and hidden row spans.
    pub filter: Option<WireFilterState>,
    /// Frozen top row count (`0..=32`).
    pub frozen_rows: u32,
    /// Frozen left column count (`0..=32`).
    pub frozen_cols: u32,
    /// Sparse `(0-based visible row, custom height px)` pairs sorted by position.
    pub custom_rows: Vec<(u32, u16)>,
    /// Sparse `(0-based visible col, custom width px)` pairs sorted by position.
    pub custom_cols: Vec<(u32, u16)>,
    /// Resolved cell style table (`styleId` in viewport v2 pages indexes this array).
    pub styles: Vec<WireCellStyle>,
    /// Total bound formulas across the workbook.
    pub formula_count: u32,
    /// Seed the sheet was generated from (`0` if not from a sample).
    pub seed: u32,
    /// How long generating/loading it took, in milliseconds.
    pub generation_ms: f64,
    /// Whether `Undo` is available.
    pub can_undo: bool,
    /// Whether `Redo` is available.
    pub can_redo: bool,
    /// Label of next `Undo` step, if any.
    pub undo_label: Option<String>,
    /// Label of next `Redo` step, if any.
    pub redo_label: Option<String>,
    /// Current highest WAL sequence number in memory.
    #[serde(with = "wire_u64")]
    #[tsify(type = "string")]
    pub wal_seq: u64,
    /// True if Safe Mode froze automatic recalculation.
    pub recalc_frozen: bool,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);

    /// The worker's monotonic clock, in milliseconds.
    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    fn performance_now() -> f64;

    /// Optional Worker panic callback (`self.__reticulaOnWasmPanic`) invoked
    /// immediately before WASM trap unwind so OPFS writes halt cleanly.
    #[wasm_bindgen(js_namespace = globalThis, js_name = __reticulaOnWasmPanic, catch)]
    fn try_notify_panic(message: &str) -> Result<(), JsValue>;
}

#[inline]
fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        performance_now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
    }
}

/// Runs once when the module is instantiated (`14-m1b.md` 7.3).
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(start)]
fn start() {
    std::panic::set_hook(Box::new(|info| {
        ENGINE_PANICKED.store(true, Ordering::SeqCst);
        let msg = format!("engine panic: {info}");
        console_error(&msg);
        let _ = try_notify_panic(&msg);
    }));
}

/// Returns `true` if a WASM panic has occurred in this module instance.
#[wasm_bindgen(js_name = hasEnginePanicked)]
#[must_use]
pub fn has_engine_panicked() -> bool {
    ENGINE_PANICKED.load(Ordering::SeqCst)
}

/// Reserves `bytes` of linear memory through the Rust global allocator in one
/// shot and immediately releases it into the allocator's free list.
#[wasm_bindgen(js_name = reserveForBytes)]
pub fn reserve_for_bytes(bytes: usize) -> bool {
    reticula_core::memory::try_reserve_heap(bytes)
}

/// Encodes the 16-byte `RTCLWAL3` header + initial `WireWalMetaV1` anchor frame.
#[wasm_bindgen(js_name = encodeWalInit)]
pub fn wasm_encode_wal_init(base_snapshot_seq: f64) -> Result<Vec<u8>, JsError> {
    let base_seq = f64_to_u64(base_snapshot_seq)?;
    encode_wal_init_v3(base_seq)
        .map_err(|e| JsError::new(&format!("WAL header encode failed: {e:?}")))
}

/// Inspects a raw `wal.log` buffer and classifies it as clean, recoverable tail,
/// or mid-file/header corruption for D-029 Safe Mode (`14-m1b.md` 2.4 & 7.3).
#[wasm_bindgen(js_name = inspectWal)]
pub fn wasm_inspect_wal(
    wal_bytes: &[u8],
    expected_base_seq: f64,
    last_flushed_seq: f64,
) -> Result<Ts<WireWalInspection>, JsError> {
    let base = f64_to_u64(expected_base_seq)?;
    let flushed = f64_to_u64(last_flushed_seq)?;
    let inspection = inspect_wal_inner(wal_bytes, base, flushed);
    Ok(Ts::from_rust(&inspection)?)
}

fn inspect_wal_inner(
    wal_bytes: &[u8],
    expected_base_seq: u64,
    last_flushed_seq: u64,
) -> WireWalInspection {
    match scan_wal_any_v3(wal_bytes, expected_base_seq, last_flushed_seq) {
        Ok(scan) => {
            let truncate_to_bytes = match scan.tail {
                WalTailDisposition::Clean => None,
                WalTailDisposition::ZeroPaddedTail { valid_bytes }
                | WalTailDisposition::TornFinalRecord { valid_bytes } => {
                    u32::try_from(valid_bytes).ok()
                }
            };
            let last_seq = scan.last_seq();
            let record_count = u32::try_from(scan.records.len()).unwrap_or(u32::MAX);
            let tail_txns = summarize_tail_records(&scan.records, 25);
            WireWalInspection::Valid {
                base_seq: scan.base_snapshot_seq,
                last_seq,
                record_count,
                truncate_to_bytes,
                tail_txns,
            }
        }
        Err(corr) => match corr {
            WalCorruptionV3::InvalidHeader => WireWalInspection::Corrupted {
                reason: String::from("WAL header magic or CRC-32 checksum is corrupted."),
                salvageable_count: 0,
                salvageable_tail_txns: Vec::new(),
            },
            WalCorruptionV3::BaseSeqMismatch { found, expected } => WireWalInspection::Corrupted {
                reason: format!(
                    "WAL base snapshot sequence mismatch (found {found:?}, expected {expected})."
                ),
                salvageable_count: 0,
                salvageable_tail_txns: Vec::new(),
            },
            WalCorruptionV3::NonContiguousSeq {
                offset,
                expected_seq,
                found_seq,
            } => WireWalInspection::Corrupted {
                reason: format!(
                    "Non-contiguous WAL sequence at byte {offset} (expected {expected_seq}, found {found_seq})."
                ),
                salvageable_count: 0,
                salvageable_tail_txns: Vec::new(),
            },
            WalCorruptionV3::MidFileCorruption {
                offset,
                last_valid_seq,
                salvageable_records,
            } => {
                let count = u32::try_from(salvageable_records.len()).unwrap_or(u32::MAX);
                let tail = summarize_tail_records(&salvageable_records, 25);
                WireWalInspection::Corrupted {
                    reason: format!(
                        "Mid-file WAL corruption at byte {offset} after sequence {last_valid_seq}."
                    ),
                    salvageable_count: count,
                    salvageable_tail_txns: tail,
                }
            }
        },
    }
}

fn summarize_tail_records(records: &[WireWalRecordV3], max_tail: usize) -> Vec<WireWalTxnSummary> {
    let skip = records.len().saturating_sub(max_tail);
    records
        .iter()
        .skip(skip)
        .map(|r| WireWalTxnSummary {
            seq: r.seq,
            #[expect(clippy::cast_precision_loss, reason = "epoch ms < 2^53 fits in f64")]
            timestamp_ms: r.timestamp_ms as f64,
            label: r.label.clone(),
        })
        .collect()
}

/// The engine: one open workbook (`FormulaEngine`), session undo/redo history,
/// WAL frame outbox, optional streaming snapshot/CSV state machines, and the
/// grid's freeze-aware viewport subscription.
#[wasm_bindgen]
pub struct Engine {
    engine: FormulaEngine,
    collab: Option<collab::CollabSession>,
    undo_stack: UndoStack,
    clipboard: Option<InternalClipboard>,
    wal_seq: u64,
    last_timestamp_ms: u64,
    pending_wal_frames: VecDeque<Vec<u8>>,
    snapshot_encoder: Option<SnapshotEncoder>,
    pending_snapshot_chunk: Option<Vec<u8>>,
    csv_reader: Option<CsvReader>,
    recalc_frozen: bool,
    seed: u32,
    generation_ms: f64,
    /// Sheet currently subscribed by the grid.
    subscribed_sheet: SheetId,
    subscribed_rows: (u32, u32),
    subscribed_cols: (u32, u32),
    subscribed_pages: Vec<Page>,
    subscribed_set: HashSet<Page>,
    /// The subscription epoch that pages are currently sent for.
    epoch: u32,
    /// Pages that are due, in the order they will be sent.
    due: VecDeque<Page>,
    /// O(1) membership set keeping `due` free of duplicates.
    due_set: HashSet<Page>,
    /// Number of style entries sent in the latest `SheetInfo` / `StylesUpdated`.
    last_sent_styles_len: usize,
    /// Pending auxiliary event (such as `StylesUpdated` when scrolling interns a new style).
    pending_aux_event: Option<EngineEvent>,
    /// Accumulated engine milliseconds spent across the active recalculation wave.
    recalc_elapsed_ms: f64,
    /// Scratch space for encoding pages.
    text: String,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("wal_seq", &self.wal_seq)
            .field("subscribed_sheet", &self.subscribed_sheet)
            .field("recalc_frozen", &self.recalc_frozen)
            .finish_non_exhaustive()
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            engine: FormulaEngine::default(),
            collab: None,
            undo_stack: UndoStack::new(),
            clipboard: None,
            wal_seq: 0,
            last_timestamp_ms: 0,
            pending_wal_frames: VecDeque::new(),
            snapshot_encoder: None,
            pending_snapshot_chunk: None,
            csv_reader: None,
            recalc_frozen: false,
            seed: 0,
            generation_ms: 0.0,
            subscribed_sheet: SheetId(0),
            subscribed_rows: (0, 0),
            subscribed_cols: (0, 0),
            subscribed_pages: Vec::new(),
            subscribed_set: HashSet::new(),
            epoch: 0,
            due: VecDeque::new(),
            due_set: HashSet::new(),
            last_sent_styles_len: 1,
            pending_aux_event: None,
            recalc_elapsed_ms: 0.0,
            text: String::new(),
        }
    }
}

#[wasm_bindgen]
impl Engine {
    /// Creates an engine with a fresh empty workbook.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Carries out a command and returns the event that answers it, if the
    /// command has one. Afterwards, drain WAL frames with
    /// [`Self::take_pending_wal_frame`], take pages until none remain, and check
    /// [`Self::has_pending_recalc`].
    ///
    /// # Errors
    ///
    /// Fails if `command` is malformed or if the engine previously panicked.
    pub fn handle(
        &mut self,
        command: &Ts<EngineCommand>,
    ) -> Result<Option<Ts<EngineEvent>>, JsError> {
        check_not_panicked()?;
        let cmd = command.to_rust()?;
        let event = self.handle_command(cmd);
        Ok(event.as_ref().map(Ts::from_rust).transpose()?)
    }

    /// Whether dirty formulas remain queued for subsequent [`EngineCommand::ContinueRecalc`]
    /// slices (always `false` while Safe Mode `recalc_frozen` is active).
    #[wasm_bindgen(js_name = hasPendingRecalc)]
    #[must_use]
    pub fn has_pending_recalc(&self) -> bool {
        !self.recalc_frozen && self.engine.has_pending_recalc()
    }

    /// Encodes the next viewport page that is due and hands it over, or returns
    /// nothing when the grid has been sent everything due.
    #[wasm_bindgen(js_name = takePage)]
    pub fn take_page(&mut self) -> Option<Vec<u8>> {
        let page = self.due.pop_front()?;
        self.due_set.remove(&page);
        let sheet_id = self.subscribed_sheet;
        self.engine.prepare_page_styles(sheet_id, page);
        let sheet = self.engine.workbook().sheet(sheet_id)?;
        if sheet.styles().entries().len() > self.last_sent_styles_len {
            self.last_sent_styles_len = sheet.styles().entries().len();
            self.pending_aux_event = Some(EngineEvent::StylesUpdated {
                sheet: sheet_id.0,
                styles: sheet
                    .styles()
                    .entries()
                    .iter()
                    .copied()
                    .map(WireCellStyle::from)
                    .collect(),
            });
        }
        let _ = sheet;
        self.engine
            .encode_viewport_page(sheet_id, page, self.epoch, &mut self.text)
    }

    /// Takes an auxiliary event (such as [`EngineEvent::StylesUpdated`] emitted
    /// if `takePage` lazily interned a cross-axis style).
    #[wasm_bindgen(js_name = takeAuxEvent)]
    pub fn take_aux_event(&mut self) -> Result<Option<Ts<EngineEvent>>, JsError> {
        let ev = self.pending_aux_event.take();
        Ok(ev.as_ref().map(Ts::from_rust).transpose()?)
    }

    /// Pops the next framed `RTCLWAL1` transaction record ready to append to
    /// OPFS `wal.log`.
    #[wasm_bindgen(js_name = takePendingWalFrame)]
    pub fn take_pending_wal_frame(&mut self) -> Option<Vec<u8>> {
        self.pending_wal_frames.pop_front()
    }

    /// Encodes a ~120-byte `SeedOrigin` snapshot stub when the current workbook
    /// is an unmodified seeded sample (`14-m1b.md` 7.1), or returns `None`.
    #[wasm_bindgen(js_name = encodeSeedStub)]
    pub fn encode_seed_stub(&self) -> Result<Option<Vec<u8>>, JsError> {
        check_not_panicked()?;
        let Some(origin) = self.engine.workbook().seed_origin() else {
            return Ok(None);
        };
        let bytes = encode_seed_stub_v1(origin, &[])
            .map_err(|e| JsError::new(&format!("encodeSeedStub failed: {e:?}")))?;
        Ok(Some(bytes))
    }

    /// Encodes a complete standalone `RTCLSNAP` v1 snapshot buffer (used for
    /// `.rtcl` file export or immediate small-workbook saves). When
    /// `compact_garbage` is true, purges tombstones and orphaned strings on the
    /// exported image without invalidating the live session's `UndoStack`.
    #[wasm_bindgen(js_name = encodeSnapshotFull)]
    pub fn encode_snapshot_full(&mut self, compact_garbage: bool) -> Result<Vec<u8>, JsError> {
        check_not_panicked()?;
        if compact_garbage {
            if !self.undo_stack.can_undo() && !self.undo_stack.can_redo() {
                self.engine.compact_local_garbage();
                let mut wb = self.engine.workbook().clone();
                wb.clear_seed_origin();
                return encode_snapshot_v1(&wb, &[])
                    .map_err(|e| JsError::new(&format!("encodeSnapshotFull failed: {e:?}")));
            }
            let mut exported_wb = self.engine.workbook().clone();
            exported_wb.clear_seed_origin();
            exported_wb.compact_local_garbage();
            return encode_snapshot_v1(&exported_wb, &[])
                .map_err(|e| JsError::new(&format!("encodeSnapshotFull failed: {e:?}")));
        }
        let mut exported_wb = self.engine.workbook().clone();
        exported_wb.clear_seed_origin();
        encode_snapshot_v1(&exported_wb, &[])
            .map_err(|e| JsError::new(&format!("encodeSnapshotFull failed: {e:?}")))
    }

    /// Starts a Copy-on-Write background `RTCLSNAP` v1 stream (`< 0.2 ms` fork)
    /// and returns the `snapshotSeq` covered by this snapshot (`14-m1b.md` 2.3).
    #[wasm_bindgen(js_name = startSnapshotStream)]
    pub fn start_snapshot_stream(&mut self) -> Result<f64, JsError> {
        check_not_panicked()?;
        let mut fork_wb = self.engine.workbook().clone();
        fork_wb.clear_seed_origin();
        self.snapshot_encoder = Some(SnapshotEncoder::new(&fork_wb, Vec::new()));
        self.pending_snapshot_chunk = None;
        #[expect(
            clippy::cast_precision_loss,
            reason = "wal_seq < 2^53 fits exactly in f64"
        )]
        Ok(self.wal_seq as f64)
    }

    /// Advances the active CoW snapshot stream by up to `max_blocks` `4096`-row
    /// column blocks; pull binary chunks via [`Self::take_snapshot_chunk`].
    #[wasm_bindgen(js_name = stepSnapshotStream)]
    pub fn step_snapshot_stream(
        &mut self,
        max_blocks: u32,
    ) -> Result<Ts<WireSnapshotStep>, JsError> {
        check_not_panicked()?;
        let Some(ref mut enc) = self.snapshot_encoder else {
            return Err(JsError::new("No active snapshot stream."));
        };
        let step = enc
            .encode_step(self.engine.workbook(), max_blocks.max(1) as usize)
            .map_err(|e| JsError::new(&format!("Snapshot step failed: {e:?}")))?;
        let wire_step = match step {
            SnapshotStep::Progress => WireSnapshotStep::Progress,
            SnapshotStep::Chunk { offset, bytes } => {
                let byte_len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                self.pending_snapshot_chunk = Some(bytes);
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "snapshot offset < 4 GiB fits in f64"
                )]
                WireSnapshotStep::Chunk {
                    offset: offset as f64,
                    byte_len,
                }
            }
            SnapshotStep::Finished {
                header_bytes,
                total_len,
            } => {
                self.pending_snapshot_chunk = Some(header_bytes);
                self.snapshot_encoder = None;
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "snapshot size < 4 GiB fits in f64"
                )]
                WireSnapshotStep::Finished {
                    total_len: total_len as f64,
                }
            }
        };
        Ok(Ts::from_rust(&wire_step)?)
    }

    /// Returns the `Uint8Array` chunk produced by the most recent
    /// [`Self::step_snapshot_stream`] `Chunk` or `Finished` step.
    #[wasm_bindgen(js_name = takeSnapshotChunk)]
    pub fn take_snapshot_chunk(&mut self) -> Option<Vec<u8>> {
        self.pending_snapshot_chunk.take()
    }

    /// Cancels any in-flight background snapshot stream.
    #[wasm_bindgen(js_name = abortSnapshotStream)]
    pub fn abort_snapshot_stream(&mut self) {
        self.snapshot_encoder = None;
        self.pending_snapshot_chunk = None;
    }

    /// Decodes a base snapshot (`RTCLSNAP` v1, `SeedOrigin` stub, or legacy `v0`)
    /// and optionally replays `wal_bytes` on top of it (`14-m1b.md` 5.5 & 7.1–7.3).
    #[expect(
        clippy::too_many_arguments,
        reason = "single atomic WASM entrypoint avoids multi-call state drift during cold open / Safe Mode"
    )]
    #[wasm_bindgen(js_name = loadSnapshotAndWal)]
    pub fn load_snapshot_and_wal(
        &mut self,
        snapshot_bytes: &[u8],
        wal_bytes: &[u8],
        expected_base_seq: f64,
        last_flushed_seq: f64,
        skip_tail_txns: u32,
        recalculate: bool,
        salvage_corrupt_mid: bool,
        compact_after_replay: bool,
    ) -> Result<Ts<WireLoadOutcome>, JsError> {
        check_not_panicked()?;
        let started = now_ms();
        let base_seq = f64_to_u64(expected_base_seq)?;
        let flushed_seq = f64_to_u64(last_flushed_seq)?;

        if snapshot_bytes.len() > 4096 {
            let _ = reticula_core::memory::try_reserve_heap(snapshot_bytes.len().saturating_mul(5));
        }

        let decoded = decode_snapshot_any(snapshot_bytes)
            .map_err(|e| JsError::new(&format!("Snapshot decode failed: {e:?}")))?;
        let mut new_engine = decoded.engine;
        let mut truncate_wal_to_bytes: Option<u32> = None;
        let mut replayed_txns: u32 = 0;
        let mut final_seq = base_seq;

        if !wal_bytes.is_empty() {
            let records = match scan_wal_any_v3(wal_bytes, base_seq, flushed_seq) {
                Ok(scan) => {
                    if let WalTailDisposition::ZeroPaddedTail { valid_bytes }
                    | WalTailDisposition::TornFinalRecord { valid_bytes } = scan.tail
                    {
                        truncate_wal_to_bytes = u32::try_from(valid_bytes).ok();
                    }
                    scan.records
                }
                Err(WalCorruptionV3::MidFileCorruption {
                    salvageable_records,
                    ..
                }) if salvage_corrupt_mid => salvageable_records,
                Err(corr) => {
                    return Err(JsError::new(&format!("WAL corruption detected: {corr:?}")));
                }
            };

            let applied_len = records.len().saturating_sub(skip_tail_txns as usize);
            if let Some(last_rec) = records.get(applied_len.saturating_sub(1))
                && applied_len > 0
            {
                final_seq = last_rec.seq;
            }
            replay_wal_records_v3(
                &mut new_engine,
                &records,
                skip_tail_txns as usize,
                recalculate,
            )
            .map_err(|e| JsError::new(&format!("WAL replay failed: {e:?}")))?;
            replayed_txns = u32::try_from(applied_len).unwrap_or(u32::MAX);

            if compact_after_replay && replayed_txns > 0 && recalculate {
                new_engine.compact_local_garbage();
            }
        }

        let _ = new_engine.take_dirty_changes();
        let first_live_sheet = new_engine
            .workbook()
            .live_sheets()
            .next()
            .map_or(SheetId(0), Sheet::id);

        self.engine = new_engine;
        self.undo_stack.clear();
        self.clipboard = None;
        self.wal_seq = final_seq;
        self.pending_wal_frames.clear();
        self.snapshot_encoder = None;
        self.pending_snapshot_chunk = None;
        self.csv_reader = None;
        self.recalc_frozen = !recalculate;
        self.seed = self
            .engine
            .workbook()
            .seed_origin()
            .and_then(|o| u32::try_from(o.seed).ok())
            .unwrap_or(0);
        self.generation_ms = now_ms() - started;
        self.reset_subscription(first_live_sheet);

        let sheet = self.sheet_info(first_live_sheet);
        let outcome = WireLoadOutcome {
            info: EngineInfo {
                kernel_version: reticula_core::KERNEL_VERSION.to_owned(),
            },
            sheet,
            replayed_txns,
            truncate_wal_to_bytes,
            recalc_frozen: self.recalc_frozen,
        };
        Ok(Ts::from_rust(&outcome)?)
    }

    /// Applies a single postcard-encoded `WireTxnV3` directly to this engine,
    /// recalculates all affected formulas, enqueues subscribed viewport pages,
    /// and returns the updated `SheetInfo` (used by pure-client AI replay
    /// playback and local selective merge).
    #[wasm_bindgen(js_name = applyWireTxnV3)]
    pub fn apply_wire_txn_v3(&mut self, bytes: &[u8]) -> Result<Ts<SheetInfo>, JsError> {
        check_not_panicked()?;
        let txn: WireTxnV3 = postcard::from_bytes(bytes)
            .map_err(|e| JsError::new(&format!("invalid WireTxnV3: {e}")))?;
        let (_, ops) = ops_from_wire_txn_v3(txn)
            .map_err(|e| JsError::new(&format!("invalid WireTxnV3 ops: {e:?}")))?;
        for op in ops {
            self.engine
                .apply_op(op)
                .map_err(|e| JsError::new(&format!("apply op failed: {e:?}")))?;
        }
        self.engine.recalc_all();
        let _ = self.engine.take_dirty_changes();
        let active = if self
            .engine
            .workbook()
            .sheet(self.subscribed_sheet)
            .is_some()
        {
            self.subscribed_sheet
        } else {
            self.engine
                .workbook()
                .live_sheets()
                .next()
                .map_or(SheetId(0), Sheet::id)
        };
        if active != self.subscribed_sheet {
            self.reset_subscription(active);
        } else {
            let pages = self.subscribed_pages.clone();
            for page in pages {
                self.enqueue_page(page);
            }
        }
        Ok(Ts::from_rust(&self.sheet_info(active))?)
    }

    /// Validates `raw_bytes` (`<= 256 MiB`, valid UTF-8) and begins row-batched
    /// RFC 4180 CSV/TSV import (`14-m1b.md` 5.6). Returns `raw_bytes.len() as u32`.
    #[wasm_bindgen(js_name = startCsvImport)]
    pub fn start_csv_import(
        &mut self,
        sheet_name: &str,
        raw_bytes: Vec<u8>,
    ) -> Result<u32, JsError> {
        check_not_panicked()?;
        if raw_bytes.len() > MAX_CSV_IMPORT_BYTES {
            return Err(JsError::new(
                "CSV file exceeds the 256 MiB local import limit.",
            ));
        }
        let total_len = u32::try_from(raw_bytes.len()).unwrap_or(u32::MAX);
        let safe_name = sanitize_sheet_title(sheet_name);
        let reader = CsvReader::new(SheetId(0), safe_name, raw_bytes, None)
            .map_err(|e| JsError::new(&format_csv_error(e)))?;
        self.csv_reader = Some(reader);
        Ok(total_len)
    }

    /// Parses up to `max_rows` (`65,536` recommended) CSV rows and emits either
    /// percentage progress or the completed [`SheetInfo`].
    #[wasm_bindgen(js_name = stepCsvImport)]
    pub fn step_csv_import(&mut self, max_rows: u32) -> Result<Ts<WireCsvImportStep>, JsError> {
        check_not_panicked()?;
        let started = now_ms();
        let Some(mut reader) = self.csv_reader.take() else {
            return Err(JsError::new("No active CSV import."));
        };
        let done = match reader.parse_rows(max_rows.max(1)) {
            Ok(done) => done,
            Err(err) => return Err(JsError::new(&format_csv_error(err))),
        };
        if !done {
            let step = WireCsvImportStep::Progress {
                rows_parsed: reader.rows_parsed(),
                percent: reader.progress_percent(),
            };
            self.csv_reader = Some(reader);
            return Ok(Ts::from_rust(&step)?);
        }

        let sheet = reader
            .finish()
            .map_err(|e| JsError::new(&format_csv_error(e)))?;
        self.engine = FormulaEngine::from_workbook_clean(Workbook::from_sheet(sheet, None));
        self.undo_stack.clear();
        self.clipboard = None;
        self.wal_seq = 0;
        self.pending_wal_frames.clear();
        self.snapshot_encoder = None;
        self.pending_snapshot_chunk = None;
        self.recalc_frozen = false;
        self.seed = 0;
        self.generation_ms = now_ms() - started;
        self.reset_subscription(SheetId(0));

        let step = WireCsvImportStep::Done {
            info: EngineInfo {
                kernel_version: reticula_core::KERNEL_VERSION.to_owned(),
            },
            sheet: self.sheet_info(SheetId(0)),
        };
        Ok(Ts::from_rust(&step)?)
    }

    /// Exports `sheet` as RFC 4180 UTF-8 CSV bytes (`14-m1b.md` 5.6).
    #[wasm_bindgen(js_name = exportCsv)]
    pub fn export_csv(&self, sheet: u32) -> Result<Vec<u8>, JsError> {
        check_not_panicked()?;
        let s = self
            .engine
            .workbook()
            .sheet(SheetId(sheet))
            .ok_or_else(|| JsError::new("Target sheet does not exist."))?;
        Ok(export_sheet_delimited(s, b',').into_bytes())
    }
}

impl Engine {
    /// Pure Rust command handler used both by WASM bindings and unit tests.
    pub fn handle_command(&mut self, command: EngineCommand) -> Option<EngineEvent> {
        let event = match command {
            EngineCommand::OpenSample { seed, rows } => Some(self.open_sample(seed, rows)),
            EngineCommand::OpenEmpty => Some(self.open_empty()),
            EngineCommand::Subscribe {
                sheet,
                epoch,
                first_row,
                row_count,
                first_col,
                col_count,
            } => {
                self.subscribe(
                    SheetId(sheet),
                    epoch,
                    (first_row, row_count),
                    (first_col, col_count),
                );
                None
            }
            EngineCommand::SwitchSheet { sheet } => Some(self.switch_sheet(SheetId(sheet))),
            EngineCommand::SetEvalEnv {
                now_serial,
                seed,
                timestamp_ms,
            } => {
                if let Some(ts) = timestamp_ms
                    && ts.is_finite()
                    && ts >= 0.0
                {
                    #[expect(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "guarded >= 0 finite"
                    )]
                    {
                        self.last_timestamp_ms = ts as u64;
                    }
                }
                self.engine.set_eval_env(now_serial, seed);
                None
            }
            EngineCommand::InspectCell {
                sheet,
                row_id,
                col_id,
            } => self.inspect_cell(SheetId(sheet), RowId(row_id), ColId(col_id)),
            EngineCommand::EditCell {
                sheet,
                row_id,
                col_id,
                raw,
            } => Some(self.edit_cell(SheetId(sheet), RowId(row_id), ColId(col_id), &raw)),
            EngineCommand::ClearCells { sheet, rows, cols } => {
                Some(self.clear_cells(SheetId(sheet), rows, cols))
            }
            EngineCommand::InsertRows {
                sheet,
                after_row_id,
                count,
            } => Some(self.insert_rows(SheetId(sheet), after_row_id, count)),
            EngineCommand::DeleteRows { sheet, rows } => {
                Some(self.delete_rows(SheetId(sheet), rows))
            }
            EngineCommand::InsertCols {
                sheet,
                after_col_id,
                count,
            } => Some(self.insert_cols(SheetId(sheet), after_col_id, count)),
            EngineCommand::DeleteCols { sheet, cols } => {
                Some(self.delete_cols(SheetId(sheet), cols))
            }
            EngineCommand::MoveRows {
                sheet,
                rows,
                after_row_id,
            } => Some(self.move_rows(SheetId(sheet), rows, after_row_id)),
            EngineCommand::MoveCols {
                sheet,
                cols,
                after_col_id,
            } => Some(self.move_cols(SheetId(sheet), cols, after_col_id)),
            EngineCommand::ResizeRows { sheet, sizes } => {
                Some(self.resize_rows(SheetId(sheet), &sizes))
            }
            EngineCommand::ResizeCols { sheet, sizes } => {
                Some(self.resize_cols(SheetId(sheet), &sizes))
            }
            EngineCommand::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            } => Some(self.set_freeze(SheetId(sheet), frozen_rows, frozen_cols)),
            EngineCommand::SetFormat {
                sheet,
                target,
                number_format,
                style,
            } => Some(self.set_format(SheetId(sheet), target, number_format, style)),
            EngineCommand::FillRange {
                sheet,
                src_first_row,
                src_row_count,
                src_first_col,
                src_col_count,
                dst_first_row,
                dst_row_count,
                dst_first_col,
                dst_col_count,
            } => Some(self.fill_range(
                SheetId(sheet),
                RectPos {
                    first_row: src_first_row,
                    row_count: src_row_count,
                    first_col: src_first_col,
                    col_count: src_col_count,
                },
                RectPos {
                    first_row: dst_first_row,
                    row_count: dst_row_count,
                    first_col: dst_first_col,
                    col_count: dst_col_count,
                },
            )),
            EngineCommand::AutoFillDown {
                sheet,
                src_first_row,
                src_row_count,
                src_first_col,
                src_col_count,
            } => Some(self.auto_fill_down(
                SheetId(sheet),
                RectPos {
                    first_row: src_first_row,
                    row_count: src_row_count,
                    first_col: src_first_col,
                    col_count: src_col_count,
                },
            )),
            EngineCommand::CopyRange {
                sheet,
                first_row,
                row_count,
                first_col,
                col_count,
                cut,
            } => Some(self.copy_selection(
                SheetId(sheet),
                RectPos {
                    first_row,
                    row_count,
                    first_col,
                    col_count,
                },
                cut,
            )),
            EngineCommand::PasteClipboard {
                sheet,
                dest_row,
                dest_row_count,
                dest_col,
                dest_col_count,
                tsv,
                html,
            } => Some(self.paste_clipboard(
                SheetId(sheet),
                RectPos {
                    first_row: dest_row,
                    row_count: dest_row_count,
                    first_col: dest_col,
                    col_count: dest_col_count,
                },
                &tsv,
                html.as_deref(),
            )),
            EngineCommand::AddSheet { name, after_sheet } => {
                Some(self.add_sheet(name, after_sheet.map(SheetId)))
            }
            EngineCommand::RenameSheet { sheet, name } => {
                Some(self.rename_sheet(SheetId(sheet), name))
            }
            EngineCommand::DeleteSheet { sheet } => Some(self.delete_sheet(SheetId(sheet))),
            EngineCommand::MoveSheet { sheet, after_sheet } => {
                Some(self.move_sheet(SheetId(sheet), after_sheet.map(SheetId)))
            }
            EngineCommand::Undo => Some(self.undo_txn()),
            EngineCommand::Redo => Some(self.redo_txn()),
            EngineCommand::AnalyzeFormula {
                sheet,
                source,
                cursor_utf16,
            } => {
                let analysis = lang_service::analyze_formula_with_spills(
                    &source,
                    cursor_utf16,
                    self.engine.workbook(),
                    SheetId(sheet),
                    Some(self.engine.spills()),
                );
                Some(EngineEvent::FormulaAnalyzed {
                    analysis: analysis.into(),
                })
            }
            EngineCommand::ToggleFormulaDollar {
                source,
                cursor_utf16,
            } => match lang_service::toggle_reference_dollar(&source, cursor_utf16) {
                Some((updated, start_utf16, end_utf16)) => {
                    Some(EngineEvent::FormulaDollarToggled {
                        changed: true,
                        source: updated,
                        start_utf16,
                        end_utf16,
                    })
                }
                None => Some(EngineEvent::FormulaDollarToggled {
                    changed: false,
                    source,
                    start_utf16: cursor_utf16,
                    end_utf16: cursor_utf16,
                }),
            },
            EngineCommand::FormatRangeRef {
                host_sheet,
                target_sheet,
                r1,
                c1,
                r2,
                c2,
            } => {
                let ts = SheetId(target_sheet);
                let pr1 = self.engine.display_to_physical_row(ts, r1).unwrap_or(r1);
                let pr2 = self.engine.display_to_physical_row(ts, r2).unwrap_or(r2);
                Some(EngineEvent::RangeRefFormatted {
                    reference: lang_service::format_sheet_range_ref(
                        self.engine.workbook(),
                        SheetId(host_sheet),
                        ts,
                        pr1,
                        c1,
                        pr2,
                        c2,
                    ),
                })
            }
            EngineCommand::JumpEdge {
                sheet,
                row,
                col,
                dir,
                select,
            } => Some(self.jump_edge(SheetId(sheet), row, col, dir, select)),
            EngineCommand::JumpUsedEnd { sheet, select } => {
                Some(self.jump_used_end(SheetId(sheet), select))
            }
            EngineCommand::ResumeRecalc => Some(self.resume_recalc()),
            EngineCommand::ContinueRecalc => self.continue_recalc(),
            EngineCommand::SetFilterTable {
                sheet,
                header_row,
                last_row,
                first_col,
                last_col,
            } => Some(self.set_filter_table(
                SheetId(sheet),
                header_row,
                last_row,
                first_col,
                last_col,
            )),
            EngineCommand::SetColumnFilter {
                sheet,
                col,
                criterion,
            } => Some(self.set_column_filter(SheetId(sheet), col, criterion.map(Into::into))),
            EngineCommand::ClearFilter { sheet } => Some(self.clear_filter(SheetId(sheet))),
            EngineCommand::GetFilterValues { sheet, col } => {
                Some(self.get_filter_values(SheetId(sheet), col))
            }
            EngineCommand::SortRange {
                sheet,
                first_row,
                last_row,
                first_col,
                last_col,
                has_header,
                keys,
            } => Some(self.sort_range(
                SheetId(sheet),
                first_row,
                last_row,
                first_col,
                last_col,
                has_header,
                &keys,
            )),
            EngineCommand::FindAll {
                sheet,
                options,
                max_results,
            } => Some(self.find_all_matches(
                SheetId(sheet),
                &options.into(),
                max_results.unwrap_or(500) as usize,
            )),
            EngineCommand::FindNext {
                sheet,
                from_row,
                from_col,
                backwards,
                options,
            } => Some(self.find_next_match(
                SheetId(sheet),
                from_row,
                from_col,
                backwards,
                &options.into(),
            )),
            EngineCommand::ReplaceAll {
                sheet,
                options,
                replacement,
            } => Some(self.replace_all_matches(SheetId(sheet), &options.into(), &replacement)),
            EngineCommand::ListNamedRanges => Some(self.list_named_ranges()),
            EngineCommand::DefineNamedRange {
                scope,
                name,
                formula,
                comment,
                hidden,
            } => Some(self.define_named_range(
                scope.map(SheetId),
                &name,
                &formula,
                comment,
                hidden.unwrap_or(false),
            )),
            EngineCommand::DeleteNamedRange { scope, name } => {
                Some(self.delete_named_range(scope.map(SheetId), &name))
            }
            EngineCommand::StateDigest => {
                let digest = self.engine.workbook().state_digest();
                Some(EngineEvent::StateDigested {
                    digest: format!(
                        "v{}:{:016x}{:016x}:{:016x}",
                        digest.version, digest.content_hi, digest.content_lo, digest.order
                    ),
                })
            }
            EngineCommand::SelfCheck => {
                let digest = determinism::to_hex(determinism::digest());
                Some(EngineEvent::SelfChecked {
                    digest: String::from_utf8_lossy(&digest).into_owned(),
                })
            }
        };
        if matches!(event, Some(EngineEvent::SheetChanged { .. }))
            && let Some(session) = &mut self.collab
        {
            session.presence(self.subscribed_sheet);
        }
        event
    }

    fn reset_subscription(&mut self, sheet_id: SheetId) {
        self.subscribed_sheet = sheet_id;
        self.subscribed_rows = (0, 0);
        self.subscribed_cols = (0, 0);
        self.subscribed_pages.clear();
        self.subscribed_set.clear();
        self.due.clear();
        self.due_set.clear();
        self.pending_aux_event = None;
        self.recalc_elapsed_ms = 0.0;
    }

    fn open_sample(&mut self, seed: u32, rows: u32) -> EngineEvent {
        self.collab = None;
        let started = now_ms();
        self.engine = FormulaEngine::open_sample(u64::from(seed), rows);
        let generation_ms = now_ms() - started;

        self.undo_stack.clear();
        self.clipboard = None;
        self.wal_seq = 0;
        self.pending_wal_frames.clear();
        self.snapshot_encoder = None;
        self.pending_snapshot_chunk = None;
        self.csv_reader = None;
        self.recalc_frozen = false;
        self.seed = seed;
        self.generation_ms = generation_ms;
        self.reset_subscription(SheetId(0));
        let _ = self.engine.take_dirty_changes();

        EngineEvent::Ready {
            info: EngineInfo {
                kernel_version: reticula_core::KERNEL_VERSION.to_owned(),
            },
            sheet: self.sheet_info(SheetId(0)),
        }
    }

    fn open_empty(&mut self) -> EngineEvent {
        self.collab = None;
        let started = now_ms();
        self.engine = FormulaEngine::new();
        let generation_ms = now_ms() - started;

        self.undo_stack.clear();
        self.clipboard = None;
        self.wal_seq = 0;
        self.pending_wal_frames.clear();
        self.snapshot_encoder = None;
        self.pending_snapshot_chunk = None;
        self.csv_reader = None;
        self.recalc_frozen = false;
        self.seed = 0;
        self.generation_ms = generation_ms;
        self.reset_subscription(SheetId(0));
        let _ = self.engine.take_dirty_changes();

        EngineEvent::Ready {
            info: EngineInfo {
                kernel_version: reticula_core::KERNEL_VERSION.to_owned(),
            },
            sheet: self.sheet_info(SheetId(0)),
        }
    }

    fn sheet_info(&mut self, sheet_id: SheetId) -> SheetInfo {
        let (info, styles_len) = build_sheet_info_from_engine(
            &mut self.engine,
            sheet_id,
            self.seed,
            self.generation_ms,
            self.undo_stack.can_undo(),
            self.undo_stack.can_redo(),
            self.undo_stack.undo_label().map(String::from),
            self.undo_stack.redo_label().map(String::from),
            self.wal_seq,
            self.recalc_frozen,
        );
        self.last_sent_styles_len = styles_len;
        info
    }

    fn subscribe(&mut self, sheet_id: SheetId, epoch: u32, rows: (u32, u32), cols: (u32, u32)) {
        let visible_rows = self.engine.visible_row_count(sheet_id);
        let (dims, frozen) = self
            .engine
            .workbook()
            .sheet(sheet_id)
            .map_or(((0, 0), (0, 0)), |s| {
                ((visible_rows, s.cols()), (s.frozen_rows(), s.frozen_cols()))
            });
        let base_range = PageRange::covering(dims, rows, cols);
        let wanted = viewport::collect_wanted_pages_with_freeze(base_range, frozen.0, frozen.1);
        let wanted_set: HashSet<Page> = wanted.iter().copied().collect();

        let sheet_changed = sheet_id != self.subscribed_sheet;
        if sheet_changed {
            self.due.clear();
            self.due_set.clear();
            self.subscribed_set.clear();
        } else {
            self.due.retain(|page| {
                let keep = wanted_set.contains(page);
                if !keep {
                    self.due_set.remove(page);
                }
                keep
            });
        }

        for &page in &wanted {
            if !self.subscribed_set.contains(&page) && self.due_set.insert(page) {
                self.due.push_back(page);
            }
        }
        self.subscribed_sheet = sheet_id;
        self.subscribed_rows = rows;
        self.subscribed_cols = cols;
        self.subscribed_pages = wanted;
        self.subscribed_set = wanted_set;
        self.epoch = epoch;
    }

    fn refresh_subscription_extent(&mut self) {
        if self.subscribed_pages.is_empty() {
            return;
        }
        let sheet_id = self.subscribed_sheet;
        let visible_rows = self.engine.visible_row_count(sheet_id);
        let (dims, frozen) = self
            .engine
            .workbook()
            .sheet(sheet_id)
            .map_or(((0, 0), (0, 0)), |s| {
                ((visible_rows, s.cols()), (s.frozen_rows(), s.frozen_cols()))
            });
        let base_range = PageRange::covering(dims, self.subscribed_rows, self.subscribed_cols);
        let wanted = viewport::collect_wanted_pages_with_freeze(base_range, frozen.0, frozen.1);
        let wanted_set: HashSet<Page> = wanted.iter().copied().collect();
        self.due.retain(|page| {
            let keep = wanted_set.contains(page);
            if !keep {
                self.due_set.remove(page);
            }
            keep
        });
        for &page in &wanted {
            if !self.subscribed_set.contains(&page) && self.due_set.insert(page) {
                self.due.push_back(page);
            }
        }
        self.subscribed_pages = wanted;
        self.subscribed_set = wanted_set;
    }

    fn enqueue_page(&mut self, page: Page) {
        if self.subscribed_set.contains(&page) && self.due_set.insert(page) {
            self.due.push_back(page);
        }
    }

    fn flush_dirty_pages(&mut self) {
        let dirty = self.engine.take_dirty_changes();
        if self.subscribed_pages.is_empty() {
            return;
        }
        if dirty.structural || dirty.cells.len() > BULK_DIRTY_PAGE_THRESHOLD {
            let pages = self.subscribed_pages.clone();
            for page in pages {
                self.enqueue_page(page);
            }
            return;
        }
        let target_sheet = self.subscribed_sheet;
        let mut to_enqueue = Vec::new();
        if let Some(sheet) = self.engine.workbook().sheet(target_sheet) {
            for (s_id, row_id, col_id) in dirty.cells {
                if s_id != target_sheet {
                    continue;
                }
                if let (Some(phys_r), Some(c_pos)) = (
                    sheet.row_axis().id_to_pos(row_id.0),
                    sheet.col_axis().id_to_pos(col_id.0),
                ) && let Some(r_pos) = self.engine.physical_to_display_row(target_sheet, phys_r)
                {
                    let page = Page {
                        row: r_pos / PAGE_ROWS,
                        col: c_pos / PAGE_COLS,
                    };
                    if self.subscribed_set.contains(&page) {
                        to_enqueue.push(page);
                    }
                    // Also refresh the immediately rightward page (`col + 1`) if
                    // left-overflow text could spill into it!
                    let right_page = Page {
                        row: r_pos / PAGE_ROWS,
                        col: (c_pos / PAGE_COLS).saturating_add(1),
                    };
                    if self.subscribed_set.contains(&right_page) {
                        to_enqueue.push(right_page);
                    }
                }
            }
        }
        for page in to_enqueue {
            self.enqueue_page(page);
        }
    }

    fn record_wal_and_mvcc(
        &mut self,
        label: &str,
        ops: &[Op<BoundFormula>],
        inverses: &[InverseOp<BoundFormula>],
    ) {
        if let Some(ref mut enc) = self.snapshot_encoder {
            for inv in inverses {
                record_inverse_on_snapshot_encoder(enc, inv);
            }
        }
        self.wal_seq = self.wal_seq.saturating_add(1);
        if let Ok(wire_ops) = ops
            .iter()
            .map(WireOpV3::try_from)
            .collect::<Result<Vec<_>, _>>()
        {
            let rec = WireWalRecordV3 {
                seq: self.wal_seq,
                timestamp_ms: self.last_timestamp_ms,
                label: String::from(label),
                ops: wire_ops,
            };
            if let Ok(frame) = encode_wal_record_v3(&rec) {
                self.pending_wal_frames.push_back(frame);
            }
        }
    }

    fn execute_user_txn(&mut self, label: String, ops: &[Op<BoundFormula>]) -> Result<(), String> {
        let (inverses, seq) = self.apply_user_ops(&label, ops)?;
        if let Some(seq) = seq {
            self.undo_stack
                .record_collab_txn(label, ops, inverses, self.engine.workbook(), seq);
        } else {
            self.undo_stack
                .record_txn(label, ops, inverses, self.engine.workbook());
        }
        Ok(())
    }

    fn apply_user_ops(
        &mut self,
        label: &str,
        ops: &[Op<BoundFormula>],
    ) -> Result<(Vec<InverseOp<BoundFormula>>, Option<u64>), String> {
        if let Some(session) = &self.collab {
            session.preflight(label, ops, self.last_timestamp_ms)?;
        }
        let inverses = self
            .engine
            .apply_txn(ops.to_vec())
            .map_err(format_op_error)?;
        if let Some(session) = &mut self.collab {
            match session.queue(
                label,
                ops.to_vec(),
                self.last_timestamp_ms,
                inverses.clone(),
            ) {
                Ok(seq) => Ok((inverses, Some(seq))),
                Err(error) => {
                    for inverse in inverses.into_iter().rev() {
                        let _ = self.engine.apply_inverse(inverse);
                    }
                    Err(format_op_error(error))
                }
            }
        } else {
            self.record_wal_and_mvcc(label, ops, &inverses);
            Ok((inverses, None))
        }
    }

    fn run_initial_recalc_slice(&mut self) -> u32 {
        if self.recalc_frozen || !self.engine.has_pending_recalc() {
            return 0;
        }
        let slice_start = now_ms();
        match self.engine.recalc_with_budget(RECALC_CHECK_INTERVAL, |_| {
            now_ms() - slice_start >= RECALC_SLICE_BUDGET_MS
        }) {
            RecalcStatus::Complete { recalculated: n } => {
                self.recalc_elapsed_ms = 0.0;
                n
            }
            RecalcStatus::InProgress { done, .. } => {
                self.recalc_elapsed_ms += now_ms() - slice_start;
                done
            }
        }
    }

    fn switch_sheet(&mut self, sheet_id: SheetId) -> EngineEvent {
        if self.engine.workbook().live_sheet(sheet_id).is_none() {
            return EngineEvent::Rejected {
                reason: String::from("Target worksheet does not exist."),
            };
        }
        self.subscribed_sheet = sheet_id;
        self.subscribed_pages.clear();
        self.subscribed_set.clear();
        self.due.clear();
        self.due_set.clear();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms: 0.0,
            recalculated: 0,
        }
    }

    fn inspect_cell(&self, sheet_id: SheetId, row_id: RowId, col_id: ColId) -> Option<EngineEvent> {
        build_cell_inspected(&self.engine, sheet_id, row_id, col_id)
    }

    fn edit_cell(
        &mut self,
        sheet_id: SheetId,
        row_id: RowId,
        col_id: ColId,
        raw: &str,
    ) -> EngineEvent {
        let started = now_ms();
        let addr_label = cell_label(self.engine.workbook(), sheet_id, row_id, col_id);
        let label = format!("Edit {addr_label}");
        let result = if self.collab.is_some() {
            self.engine
                .build_cell_edit(sheet_id, row_id, col_id, raw)
                .map_err(format_edit_error)
                .and_then(|op| self.execute_user_txn(label, &[op]))
                .map(|()| self.engine.invalidate_volatile_after_edit())
        } else {
            self.edit_local_cell(sheet_id, row_id, col_id, raw, label)
        };
        if let Err(reason) = result {
            return EngineEvent::Rejected { reason };
        }

        let apply_ms = now_ms() - started;
        self.recalc_elapsed_ms += apply_ms;
        self.flush_dirty_pages();

        if self.recalc_frozen || !self.engine.has_pending_recalc() {
            let elapsed_ms = core::mem::take(&mut self.recalc_elapsed_ms);
            let sheet = self.sheet_info(self.subscribed_sheet);
            return EngineEvent::RecalcDone {
                recalculated: 0,
                elapsed_ms,
                sheet,
            };
        }
        self.step_recalc_slice()
    }

    fn edit_local_cell(
        &mut self,
        sheet_id: SheetId,
        row_id: RowId,
        col_id: ColId,
        raw: &str,
        label: String,
    ) -> Result<(), String> {
        let inv = self
            .engine
            .edit_cell_raw(sheet_id, row_id, col_id, raw)
            .map_err(format_edit_error)?;
        // Preserve the v1 WAL and redo representation of a local cell edit.
        let post = self
            .engine
            .workbook()
            .cell_snapshot(sheet_id, row_id, col_id);
        let ops = [Op::SetCells {
            sheet: sheet_id,
            edits: vec![reticula_core::ops::CellEdit {
                row: row_id,
                col: col_id,
                input: post
                    .as_ref()
                    .map_or(CellInput::Empty, |cell| cell.input.clone()),
                format: post
                    .and_then(|cell| cell.explicit_format)
                    .map_or(FormatPatch::Keep, FormatPatch::Set),
            }],
        }];
        self.record_wal_and_mvcc(&label, &ops, core::slice::from_ref(&inv));
        self.undo_stack
            .record_txn(label, &ops, vec![inv], self.engine.workbook());
        Ok(())
    }

    fn clear_cells(
        &mut self,
        sheet_id: SheetId,
        rows: Vec<WireIdRun>,
        cols: Vec<WireIdRun>,
    ) -> EngineEvent {
        let started = now_ms();
        let op = Op::ClearRange {
            sheet: sheet_id,
            rows: rows.into_iter().map(IdRun::from).collect(),
            cols: cols.into_iter().map(IdRun::from).collect(),
        };
        if let Err(err) = self.execute_user_txn(String::from("Clear cells"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        self.recalc_elapsed_ms += now_ms() - started;
        self.flush_dirty_pages();

        if self.recalc_frozen || !self.engine.has_pending_recalc() {
            let elapsed_ms = core::mem::take(&mut self.recalc_elapsed_ms);
            let sheet = self.sheet_info(self.subscribed_sheet);
            return EngineEvent::RecalcDone {
                recalculated: 0,
                elapsed_ms,
                sheet,
            };
        }
        self.step_recalc_slice()
    }

    fn insert_rows(
        &mut self,
        sheet_id: SheetId,
        after_row_id: Option<u64>,
        count: u32,
    ) -> EngineEvent {
        if count == 0 {
            return EngineEvent::Rejected {
                reason: String::from("Row count must be at least 1."),
            };
        }
        let started = now_ms();
        let ids = match self.collab.as_mut() {
            Some(session) => session.machine.alloc_row_run(count),
            None => self.engine.allocator_mut().alloc_run(count),
        };
        let Some(ids) = ids else {
            return EngineEvent::Rejected {
                reason: String::from("Identifier counter exhausted."),
            };
        };
        let op = Op::InsertRows {
            sheet: sheet_id,
            after: after_row_id.map(RowId),
            ids,
        };
        let label = if count == 1 {
            String::from("Insert row")
        } else {
            format!("Insert {count} rows")
        };
        if let Err(err) = self.execute_user_txn(label, &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let recalculated = self.run_initial_recalc_slice();
        let action_ms = now_ms() - started;
        self.refresh_subscription_extent();
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms,
            recalculated,
        }
    }

    fn delete_rows(&mut self, sheet_id: SheetId, rows: Vec<WireIdRun>) -> EngineEvent {
        if rows.is_empty() {
            return EngineEvent::Rejected {
                reason: String::from("Select at least one row to delete."),
            };
        }
        let started = now_ms();
        let op = Op::DeleteRows {
            sheet: sheet_id,
            ids: rows.into_iter().map(IdRun::from).collect(),
        };
        if let Err(err) = self.execute_user_txn(String::from("Delete rows"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let recalculated = self.run_initial_recalc_slice();
        let action_ms = now_ms() - started;
        self.refresh_subscription_extent();
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms,
            recalculated,
        }
    }

    fn insert_cols(
        &mut self,
        sheet_id: SheetId,
        after_col_id: Option<u64>,
        count: u32,
    ) -> EngineEvent {
        if count == 0 {
            return EngineEvent::Rejected {
                reason: String::from("Column count must be at least 1."),
            };
        }
        let started = now_ms();
        let ids = match self.collab.as_mut() {
            Some(session) => session.machine.alloc_col_run(count),
            None => self.engine.allocator_mut().alloc_run(count),
        };
        let Some(ids) = ids else {
            return EngineEvent::Rejected {
                reason: String::from("Identifier counter exhausted."),
            };
        };
        let op = Op::InsertCols {
            sheet: sheet_id,
            after: after_col_id.map(ColId),
            ids,
        };
        let label = if count == 1 {
            String::from("Insert column")
        } else {
            format!("Insert {count} columns")
        };
        if let Err(err) = self.execute_user_txn(label, &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let recalculated = self.run_initial_recalc_slice();
        let action_ms = now_ms() - started;
        self.refresh_subscription_extent();
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms,
            recalculated,
        }
    }

    fn delete_cols(&mut self, sheet_id: SheetId, cols: Vec<WireIdRun>) -> EngineEvent {
        if cols.is_empty() {
            return EngineEvent::Rejected {
                reason: String::from("Select at least one column to delete."),
            };
        }
        let started = now_ms();
        let op = Op::DeleteCols {
            sheet: sheet_id,
            ids: cols.into_iter().map(IdRun::from).collect(),
        };
        if let Err(err) = self.execute_user_txn(String::from("Delete columns"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let recalculated = self.run_initial_recalc_slice();
        let action_ms = now_ms() - started;
        self.refresh_subscription_extent();
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms,
            recalculated,
        }
    }

    fn move_rows(
        &mut self,
        sheet_id: SheetId,
        rows: Vec<WireIdRun>,
        after_row_id: Option<u64>,
    ) -> EngineEvent {
        if rows.is_empty() {
            return EngineEvent::Rejected {
                reason: String::from("Select at least one row to move."),
            };
        }
        let started = now_ms();
        let op = Op::MoveRows {
            sheet: sheet_id,
            ids: rows.into_iter().map(IdRun::from).collect(),
            after: after_row_id.map(RowId),
        };
        if let Err(err) = self.execute_user_txn(String::from("Move rows"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let recalculated = self.run_initial_recalc_slice();
        let action_ms = now_ms() - started;
        self.refresh_subscription_extent();
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms,
            recalculated,
        }
    }

    fn move_cols(
        &mut self,
        sheet_id: SheetId,
        cols: Vec<WireIdRun>,
        after_col_id: Option<u64>,
    ) -> EngineEvent {
        if cols.is_empty() {
            return EngineEvent::Rejected {
                reason: String::from("Select at least one column to move."),
            };
        }
        let started = now_ms();
        let op = Op::MoveCols {
            sheet: sheet_id,
            ids: cols.into_iter().map(IdRun::from).collect(),
            after: after_col_id.map(ColId),
        };
        if let Err(err) = self.execute_user_txn(String::from("Move columns"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let recalculated = self.run_initial_recalc_slice();
        let action_ms = now_ms() - started;
        self.refresh_subscription_extent();
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms,
            recalculated,
        }
    }

    fn resize_rows(&mut self, sheet_id: SheetId, sizes: &[WireAxisSize]) -> EngineEvent {
        let started = now_ms();
        let op = Op::ResizeRows {
            sheet: sheet_id,
            sizes: sizes
                .iter()
                .map(|s| (IdRun::from(s.run), s.size_px))
                .collect(),
        };
        if let Err(err) = self.execute_user_txn(String::from("Resize rows"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let action_ms = now_ms() - started;
        let _ = self.engine.take_dirty_changes();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms,
            recalculated: 0,
        }
    }

    fn resize_cols(&mut self, sheet_id: SheetId, sizes: &[WireAxisSize]) -> EngineEvent {
        let started = now_ms();
        let op = Op::ResizeCols {
            sheet: sheet_id,
            sizes: sizes
                .iter()
                .map(|s| (IdRun::from(s.run), s.size_px))
                .collect(),
        };
        if let Err(err) = self.execute_user_txn(String::from("Resize columns"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let action_ms = now_ms() - started;
        let _ = self.engine.take_dirty_changes();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms,
            recalculated: 0,
        }
    }

    fn set_freeze(&mut self, sheet_id: SheetId, frozen_rows: u32, frozen_cols: u32) -> EngineEvent {
        let started = now_ms();
        let op = Op::SetFreeze {
            sheet: sheet_id,
            frozen_rows,
            frozen_cols,
        };
        if let Err(err) = self.execute_user_txn(String::from("Freeze panes"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let action_ms = now_ms() - started;
        let _ = self.engine.take_dirty_changes();
        self.refresh_subscription_extent();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms,
            recalculated: 0,
        }
    }

    fn set_format(
        &mut self,
        sheet_id: SheetId,
        target: WireFormatTarget,
        number_format: WireFormatPatch,
        style: WireStylePatch,
    ) -> EngineEvent {
        let started = now_ms();
        let nf_patch = match number_format {
            WireFormatPatch::Keep => FormatPatch::Keep,
            WireFormatPatch::Clear => FormatPatch::Clear,
            WireFormatPatch::Set { code } => match parse_format_code(&code) {
                Some(fmt) => FormatPatch::Set(fmt),
                None => {
                    return EngineEvent::Rejected {
                        reason: format!("Unsupported number format code: {code}"),
                    };
                }
            },
        };
        let op = Op::SetFormat {
            sheet: sheet_id,
            target: target.into(),
            number_format: nf_patch,
            style: style.into(),
        };
        if let Err(err) = self.execute_user_txn(String::from("Format cells"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        // Pre-intern styles BEFORE encoding dirty pages so `style_id` in pages matches `SheetInfo.styles`!
        let sheet_meta = self.sheet_info(sheet_id);
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: sheet_meta,
            action_ms: now_ms() - started,
            recalculated: 0,
        }
    }

    fn map_display_rect_to_physical(&self, sheet_id: SheetId, rect: RectPos) -> Option<RectPos> {
        if self.engine.filter_state(sheet_id).is_none() {
            return Some(rect);
        }
        let pr1 = self
            .engine
            .display_to_physical_row(sheet_id, rect.first_row)?;
        let last_disp = rect
            .first_row
            .saturating_add(rect.row_count.saturating_sub(1));
        let pr2 = self.engine.display_to_physical_row(sheet_id, last_disp)?;
        Some(RectPos {
            first_row: pr1,
            row_count: pr2.saturating_sub(pr1).saturating_add(1),
            first_col: rect.first_col,
            col_count: rect.col_count,
        })
    }

    fn nearest_visible_display_row(&self, sheet_id: SheetId, physical_row: u32) -> u32 {
        if self.engine.filter_state(sheet_id).is_none() {
            return physical_row;
        }
        if let Some(d) = self.engine.physical_to_display_row(sheet_id, physical_row) {
            return d;
        }
        let Some(sheet) = self.engine.workbook().sheet(sheet_id) else {
            return 0;
        };
        let mut r = physical_row;
        while r > 0 {
            r = r.saturating_sub(1);
            if let Some(d) = self.engine.physical_to_display_row(sheet_id, r) {
                return d;
            }
        }
        let max_r = sheet.rows().saturating_sub(1);
        for candidate in physical_row..=max_r {
            if let Some(d) = self.engine.physical_to_display_row(sheet_id, candidate) {
                return d;
            }
        }
        0
    }

    fn fill_range(&mut self, sheet_id: SheetId, src: RectPos, dst: RectPos) -> EngineEvent {
        let (Some(phys_src), Some(phys_dst)) = (
            self.map_display_rect_to_physical(sheet_id, src),
            self.map_display_rect_to_physical(sheet_id, dst),
        ) else {
            return EngineEvent::Rejected {
                reason: String::from("Fill range falls outside visible rows."),
            };
        };
        self.fill_range_physical(sheet_id, phys_src, phys_dst)
    }

    fn fill_range_physical(
        &mut self,
        sheet_id: SheetId,
        src: RectPos,
        dst: RectPos,
    ) -> EngineEvent {
        let started = now_ms();
        let ops = match build_fill_ops(self.engine.workbook(), sheet_id, src, dst) {
            Ok(ops) => ops,
            Err(reason) => return EngineEvent::Rejected { reason },
        };
        if ops.is_empty() {
            return EngineEvent::SheetChanged {
                sheet: self.sheet_info(sheet_id),
                action_ms: 0.0,
                recalculated: 0,
            };
        }
        if let Err(err) = self.execute_user_txn(String::from("Auto-fill"), &ops) {
            return EngineEvent::Rejected { reason: err };
        }
        let recalculated = self.run_initial_recalc_slice();
        let sheet_meta = self.sheet_info(sheet_id);
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: sheet_meta,
            action_ms: now_ms() - started,
            recalculated,
        }
    }

    fn auto_fill_down(&mut self, sheet_id: SheetId, src: RectPos) -> EngineEvent {
        let Some(phys_src) = self.map_display_rect_to_physical(sheet_id, src) else {
            return EngineEvent::SheetChanged {
                sheet: self.sheet_info(sheet_id),
                action_ms: 0.0,
                recalculated: 0,
            };
        };
        let Some(end_row_incl) = find_autofill_end_row(self.engine.workbook(), sheet_id, phys_src)
        else {
            return EngineEvent::SheetChanged {
                sheet: self.sheet_info(sheet_id),
                action_ms: 0.0,
                recalculated: 0,
            };
        };
        let dst = RectPos {
            first_row: phys_src.first_row,
            row_count: end_row_incl
                .saturating_add(1)
                .saturating_sub(phys_src.first_row),
            first_col: phys_src.first_col,
            col_count: phys_src.col_count,
        };
        self.fill_range_physical(sheet_id, phys_src, dst)
    }

    fn copy_selection(&mut self, sheet_id: SheetId, rect: RectPos, cut: bool) -> EngineEvent {
        let Some(phys_rect) = self.map_display_rect_to_physical(sheet_id, rect) else {
            return EngineEvent::Rejected {
                reason: String::from("Copy selection falls outside visible rows."),
            };
        };
        match copy_range(self.engine.workbook(), sheet_id, phys_rect, cut) {
            Ok((payload, internal)) => {
                self.clipboard = Some(internal);
                EngineEvent::ClipboardCopied {
                    sheet: sheet_id.0,
                    first_row: rect.first_row,
                    row_count: rect.row_count,
                    first_col: rect.first_col,
                    col_count: rect.col_count,
                    cut,
                    tsv: payload.tsv,
                    html: payload.html,
                    fingerprint: payload.fingerprint,
                }
            }
            Err(reason) => EngineEvent::Rejected { reason },
        }
    }

    fn paste_clipboard(
        &mut self,
        sheet_id: SheetId,
        dest: RectPos,
        tsv: &str,
        html: Option<&str>,
    ) -> EngineEvent {
        let started = now_ms();
        let Some(phys_dest) = self.map_display_rect_to_physical(sheet_id, dest) else {
            return EngineEvent::Rejected {
                reason: String::from("Paste target falls outside visible rows."),
            };
        };
        let mut clip_clone = self.clipboard.clone();
        let (ops, applied_rect) = match build_paste_ops(
            self.engine.workbook(),
            sheet_id,
            phys_dest,
            tsv,
            html,
            &mut clip_clone,
        ) {
            Ok(res) => res,
            Err(reason) => return EngineEvent::Rejected { reason },
        };
        if !ops.is_empty() {
            if let Err(err) = self.execute_user_txn(String::from("Paste"), &ops) {
                return EngineEvent::Rejected { reason: err };
            }
            self.clipboard = clip_clone;
        }
        let recalculated = self.run_initial_recalc_slice();
        let dest_first_row = self
            .engine
            .physical_to_display_row(sheet_id, applied_rect.first_row)
            .unwrap_or(dest.first_row);
        let sheet_meta = self.sheet_info(sheet_id);
        self.flush_dirty_pages();
        EngineEvent::ClipboardPasted {
            sheet: sheet_meta,
            dest_first_row,
            dest_row_count: applied_rect.row_count,
            dest_first_col: applied_rect.first_col,
            dest_col_count: applied_rect.col_count,
            recalculated,
            elapsed_ms: now_ms() - started,
        }
    }

    fn add_sheet(&mut self, name: Option<String>, after_sheet: Option<SheetId>) -> EngineEvent {
        let started = now_ms();
        let new_id = match self.collab.as_mut() {
            Some(session) => session.machine.alloc_sheet_id(),
            None => Some(self.engine.alloc_sheet_id()),
        };
        let Some(new_id) = new_id else {
            return EngineEvent::Rejected {
                reason: "Collaborative worksheet identifier limit reached.".into(),
            };
        };
        let default_num = self
            .engine
            .workbook()
            .live_sheets()
            .count()
            .saturating_add(1);
        let req_name = name.unwrap_or_else(|| format!("Sheet{default_num}"));
        let after =
            after_sheet.or_else(|| self.engine.workbook().live_sheets().last().map(Sheet::id));
        let op = Op::AddSheet {
            sheet: new_id,
            name: req_name,
            after,
        };
        if let Err(err) = self.execute_user_txn(String::from("Add sheet"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let _ = self.engine.take_dirty_changes();
        self.subscribed_sheet = new_id;
        self.subscribed_pages.clear();
        self.subscribed_set.clear();
        self.due.clear();
        self.due_set.clear();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(new_id),
            action_ms: now_ms() - started,
            recalculated: 0,
        }
    }

    fn rename_sheet(&mut self, sheet_id: SheetId, name: String) -> EngineEvent {
        let started = now_ms();
        let op = Op::RenameSheet {
            sheet: sheet_id,
            name,
        };
        if let Err(err) = self.execute_user_txn(String::from("Rename sheet"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(self.subscribed_sheet),
            action_ms: now_ms() - started,
            recalculated: 0,
        }
    }

    fn delete_sheet(&mut self, sheet_id: SheetId) -> EngineEvent {
        let started = now_ms();
        let op = Op::DeleteSheet { sheet: sheet_id };
        if let Err(err) = self.execute_user_txn(String::from("Delete sheet"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let recalculated = self.run_initial_recalc_slice();
        if self.subscribed_sheet == sheet_id
            && let Some(next_live) = self.engine.workbook().live_sheets().next().map(Sheet::id)
        {
            self.subscribed_sheet = next_live;
            self.subscribed_pages.clear();
            self.subscribed_set.clear();
            self.due.clear();
            self.due_set.clear();
        }
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(self.subscribed_sheet),
            action_ms: now_ms() - started,
            recalculated,
        }
    }

    fn move_sheet(&mut self, sheet_id: SheetId, after_sheet: Option<SheetId>) -> EngineEvent {
        let started = now_ms();
        let op = Op::MoveSheet {
            sheet: sheet_id,
            after: after_sheet,
        };
        if let Err(err) = self.execute_user_txn(String::from("Move sheet"), &[op]) {
            return EngineEvent::Rejected { reason: err };
        }
        let _ = self.engine.take_dirty_changes();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(self.subscribed_sheet),
            action_ms: now_ms() - started,
            recalculated: 0,
        }
    }

    fn undo_txn(&mut self) -> EngineEvent {
        let started = now_ms();
        let Some(mut entry) = self.undo_stack.pop_undo() else {
            return EngineEvent::SheetChanged {
                sheet: self.sheet_info(self.subscribed_sheet),
                action_ms: 0.0,
                recalculated: 0,
            };
        };
        let ops = entry.undo_ops.clone();
        match self.apply_user_ops(&format!("Undo: {}", entry.label), &ops) {
            Ok((inverses, seq)) => {
                if let Some(seq) = seq {
                    entry.collab_seq = Some(seq);
                    entry.redo_ops = collab_compensations(&ops, inverses, self.engine.workbook())
                        .unwrap_or_default();
                }
                if !entry.redo_ops.is_empty() {
                    self.undo_stack.push_redo(entry);
                }
            }
            Err(err) => {
                self.undo_stack.push_undo_back(entry);
                return EngineEvent::Rejected { reason: err };
            }
        }
        let recalculated = self.run_initial_recalc_slice();
        self.ensure_subscribed_sheet_live();
        self.refresh_subscription_extent();
        let sheet_meta = self.sheet_info(self.subscribed_sheet);
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: sheet_meta,
            action_ms: now_ms() - started,
            recalculated,
        }
    }

    fn redo_txn(&mut self) -> EngineEvent {
        let started = now_ms();
        let Some(mut entry) = self.undo_stack.pop_redo() else {
            return EngineEvent::SheetChanged {
                sheet: self.sheet_info(self.subscribed_sheet),
                action_ms: 0.0,
                recalculated: 0,
            };
        };
        let ops = entry.redo_ops.clone();
        match self.apply_user_ops(&format!("Redo: {}", entry.label), &ops) {
            Ok((inverses, seq)) => {
                if let Some(seq) = seq {
                    entry.collab_seq = Some(seq);
                    entry.undo_ops = collab_compensations(&ops, inverses, self.engine.workbook())
                        .unwrap_or_default();
                }
                if !entry.undo_ops.is_empty() {
                    self.undo_stack.push_undo_back(entry);
                }
            }
            Err(err) => {
                self.undo_stack.push_redo(entry);
                return EngineEvent::Rejected { reason: err };
            }
        }
        let recalculated = self.run_initial_recalc_slice();
        self.ensure_subscribed_sheet_live();
        self.refresh_subscription_extent();
        let sheet_meta = self.sheet_info(self.subscribed_sheet);
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: sheet_meta,
            action_ms: now_ms() - started,
            recalculated,
        }
    }

    fn ensure_subscribed_sheet_live(&mut self) {
        if self
            .engine
            .workbook()
            .live_sheet(self.subscribed_sheet)
            .is_none()
            && let Some(first_live) = self.engine.workbook().live_sheets().next().map(Sheet::id)
        {
            self.subscribed_sheet = first_live;
            self.subscribed_pages.clear();
            self.subscribed_set.clear();
            self.due.clear();
            self.due_set.clear();
        }
    }

    fn jump_edge(
        &self,
        sheet_id: SheetId,
        row: u32,
        col: u32,
        dir: JumpDirection,
        select: bool,
    ) -> EngineEvent {
        let Some(sheet) = self.engine.workbook().sheet(sheet_id) else {
            return EngineEvent::EdgeJumped {
                sheet: sheet_id.0,
                row,
                col,
                select,
            };
        };
        let rows = sheet.rows();
        let cols = sheet.cols();
        if rows == 0 || cols == 0 {
            return EngineEvent::EdgeJumped {
                sheet: sheet_id.0,
                row: 0,
                col: 0,
                select,
            };
        }
        let phys_row = self
            .engine
            .display_to_physical_row(sheet_id, row)
            .unwrap_or(row);
        let r0 = phys_row.min(rows.saturating_sub(1));
        let c0 = col.min(cols.saturating_sub(1));
        let (nr, nc) = compute_jump_target(self.engine.workbook(), sheet, sheet_id, r0, c0, dir);
        let disp_nr = self.nearest_visible_display_row(sheet_id, nr);
        EngineEvent::EdgeJumped {
            sheet: sheet_id.0,
            row: disp_nr,
            col: nc,
            select,
        }
    }

    fn jump_used_end(&self, sheet_id: SheetId, select: bool) -> EngineEvent {
        let (mut max_r, mut max_c) = self
            .engine
            .workbook()
            .sheet(sheet_id)
            .and_then(Sheet::used_extent)
            .unwrap_or((0, 0));
        if let Some(sheet) = self.engine.workbook().sheet(sheet_id) {
            let start = (sheet_id, RowId(0), ColId(0));
            let end = (sheet_id, RowId(u64::MAX), ColId(u64::MAX));
            for ((_, r_id, c_id), _) in self.engine.workbook().formulas().range(start..=end) {
                if let (Some(rp), Some(cp)) = (
                    sheet.row_axis().id_to_pos(r_id.0),
                    sheet.col_axis().id_to_pos(c_id.0),
                ) {
                    max_r = max_r.max(rp);
                    max_c = max_c.max(cp);
                }
            }
        }
        let disp_max_r = self.nearest_visible_display_row(sheet_id, max_r);
        EngineEvent::EdgeJumped {
            sheet: sheet_id.0,
            row: disp_max_r,
            col: max_c,
            select,
        }
    }

    fn set_filter_table(
        &mut self,
        sheet_id: SheetId,
        header_row: u32,
        last_row: u32,
        first_col: u32,
        last_col: u32,
    ) -> EngineEvent {
        let started = now_ms();
        let phys_header = self
            .engine
            .display_to_physical_row(sheet_id, header_row)
            .unwrap_or(header_row);
        let phys_last = self
            .engine
            .display_to_physical_row(sheet_id, last_row)
            .unwrap_or(last_row);
        if !self
            .engine
            .set_filter_table(sheet_id, phys_header, phys_last, first_col, last_col)
        {
            return EngineEvent::Rejected {
                reason: String::from("Cannot apply AutoFilter to the selected range."),
            };
        }
        let recalculated = self.run_initial_recalc_slice();
        self.refresh_subscription_extent();
        let sheet_meta = self.sheet_info(sheet_id);
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: sheet_meta,
            action_ms: now_ms() - started,
            recalculated,
        }
    }

    fn set_column_filter(
        &mut self,
        sheet_id: SheetId,
        col: u32,
        criterion: Option<ColFilterCriterion>,
    ) -> EngineEvent {
        let started = now_ms();
        if !self.engine.set_column_filter(sheet_id, col, criterion) {
            return EngineEvent::Rejected {
                reason: String::from("Column filter outside active AutoFilter bounds."),
            };
        }
        let recalculated = self.run_initial_recalc_slice();
        self.refresh_subscription_extent();
        let sheet_meta = self.sheet_info(sheet_id);
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: sheet_meta,
            action_ms: now_ms() - started,
            recalculated,
        }
    }

    fn clear_filter(&mut self, sheet_id: SheetId) -> EngineEvent {
        let started = now_ms();
        self.engine.clear_filter(sheet_id);
        let recalculated = self.run_initial_recalc_slice();
        self.refresh_subscription_extent();
        let sheet_meta = self.sheet_info(sheet_id);
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: sheet_meta,
            action_ms: now_ms() - started,
            recalculated,
        }
    }

    fn get_filter_values(&self, sheet_id: SheetId, col: u32) -> EngineEvent {
        let distinct = self.engine.distinct_filter_values(sheet_id, col, 10_000);
        EngineEvent::FilterValues {
            sheet: sheet_id.0,
            col,
            values: distinct
                .values
                .into_iter()
                .map(|(value, count)| WireDistinctValue { value, count })
                .collect(),
            blank_count: distinct.blank_count,
            truncated: distinct.truncated,
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "maps wire SortRange args cleanly"
    )]
    fn sort_range(
        &mut self,
        sheet_id: SheetId,
        first_row: u32,
        last_row: u32,
        first_col: u32,
        last_col: u32,
        has_header: Option<bool>,
        keys: &[WireSortKey],
    ) -> EngineEvent {
        let started = now_ms();
        let phys_r1 = self
            .engine
            .display_to_physical_row(sheet_id, first_row)
            .unwrap_or(first_row);
        let phys_r2 = self
            .engine
            .display_to_physical_row(sheet_id, last_row)
            .unwrap_or(last_row);
        let (r1, r2) = (phys_r1.min(phys_r2), phys_r1.max(phys_r2));
        let (c1, c2) = (first_col.min(last_col), first_col.max(last_col));
        let sort_keys: Vec<SortKey> = keys.iter().copied().map(Into::into).collect();
        let op_opt = {
            let Some(sheet) = self.engine.workbook().live_sheet(sheet_id) else {
                return EngineEvent::Rejected {
                    reason: String::from("Target sheet not found."),
                };
            };
            let detected_header =
                has_header.unwrap_or_else(|| sort::detect_header_row(sheet, r1, r2, c1, c2));
            sort::plan_sort(
                sheet,
                r1,
                r2,
                c1,
                c2,
                detected_header,
                &sort_keys,
                Some(self.engine.spills()),
            )
        };
        if let Some(op) = op_opt
            && let Err(reason) = self.execute_user_txn(String::from("Sort"), &[op])
        {
            return EngineEvent::Rejected { reason };
        }
        let recalculated = self.run_initial_recalc_slice();
        self.refresh_subscription_extent();
        let sheet_meta = self.sheet_info(sheet_id);
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: sheet_meta,
            action_ms: now_ms() - started,
            recalculated,
        }
    }

    fn search_match_to_wire(&self, m: search::SearchMatch) -> WireSearchMatch {
        let row = self.nearest_visible_display_row(m.sheet, m.row_pos);
        WireSearchMatch {
            sheet: m.sheet.0,
            sheet_name: m.sheet_name,
            row,
            physical_row: m.row_pos,
            col: m.col_pos,
            row_id: m.row_id.0,
            col_id: m.col_id.0,
            text: m.text,
            is_formula: m.is_formula,
        }
    }

    fn find_all_matches(
        &self,
        sheet_id: SheetId,
        opts: &FindOptions,
        max_results: usize,
    ) -> EngineEvent {
        match search::find_all(&self.engine, sheet_id, opts, max_results) {
            Ok(res) => {
                let total = u32::try_from(res.matches.len()).unwrap_or(u32::MAX);
                let matches = res
                    .matches
                    .into_iter()
                    .map(|m| self.search_match_to_wire(m))
                    .collect();
                EngineEvent::SearchResults {
                    matches,
                    total,
                    truncated: res.truncated,
                }
            }
            Err(err) => EngineEvent::Rejected {
                reason: err.to_string(),
            },
        }
    }

    fn find_next_match(
        &self,
        sheet_id: SheetId,
        from_row: u32,
        from_col: u32,
        backwards: bool,
        opts: &FindOptions,
    ) -> EngineEvent {
        let phys_row = self
            .engine
            .display_to_physical_row(sheet_id, from_row)
            .unwrap_or(from_row);
        match search::find_next(&self.engine, sheet_id, phys_row, from_col, backwards, opts) {
            Ok(found) => EngineEvent::SearchNavigated {
                found: found.map(|m| self.search_match_to_wire(m)),
            },
            Err(err) => EngineEvent::Rejected {
                reason: err.to_string(),
            },
        }
    }

    fn replace_all_matches(
        &mut self,
        sheet_id: SheetId,
        opts: &FindOptions,
        replacement: &str,
    ) -> EngineEvent {
        let plan = match search::plan_replace_all(
            &self.engine,
            sheet_id,
            opts,
            replacement,
            search::MAX_REPLACE_BATCH,
        ) {
            Ok(plan) => plan,
            Err(err) => {
                return EngineEvent::Rejected {
                    reason: err.to_string(),
                };
            }
        };
        if !plan.ops.is_empty() {
            if let Err(reason) = self.execute_user_txn(String::from("Replace All"), &plan.ops) {
                return EngineEvent::Rejected { reason };
            }
            self.engine.reapply_filter(sheet_id);
        }
        let _ = self.run_initial_recalc_slice();
        self.refresh_subscription_extent();
        let sheet_meta = self.sheet_info(sheet_id);
        self.flush_dirty_pages();
        EngineEvent::ReplaceCompleted {
            replaced_cells: plan.replaced_cells,
            skipped_invalid_formulas: plan.skipped_invalid_formulas,
            truncated: plan.truncated,
            sheet: sheet_meta,
        }
    }

    fn list_named_ranges(&self) -> EngineEvent {
        let wb = self.engine.workbook();
        let fallback_sheet = wb.live_sheets().next().map_or(SheetId(0), Sheet::id);
        let items = wb
            .named_ranges()
            .iter()
            .map(|(k, def)| {
                let host = k.scope.unwrap_or(fallback_sheet);
                WireNamedRangeInfo {
                    scope: k.scope.map(|s| s.0),
                    name: def.name.clone(),
                    formula: def.formula.unparse(wb, host),
                    comment: def.comment.clone(),
                    hidden: def.hidden,
                }
            })
            .collect();
        EngineEvent::NamedRangesListed { items }
    }

    fn define_named_range(
        &mut self,
        scope: Option<SheetId>,
        name: &str,
        formula_src: &str,
        comment: Option<String>,
        hidden: bool,
    ) -> EngineEvent {
        let started = now_ms();
        let trimmed = name.trim();
        if !is_valid_named_range_name(trimmed) {
            return EngineEvent::Rejected {
                reason: format!("Invalid named range identifier '{trimmed}'."),
            };
        }
        let raw_with_eq = if formula_src.trim_start().starts_with('=') {
            formula_src.to_owned()
        } else {
            format!("={}", formula_src.trim())
        };
        let host_sheet = scope.unwrap_or(self.subscribed_sheet);
        let bound = match BoundFormula::parse_and_bind(
            &raw_with_eq,
            FormulaMode::Dynamic,
            self.engine.workbook(),
            host_sheet,
        ) {
            Ok(f) => f,
            Err(err) => {
                return EngineEvent::Rejected {
                    reason: format!("Invalid named range formula: {err:?}"),
                };
            }
        };
        let op = Op::DefineNamedRange {
            scope,
            name: String::from(trimmed),
            formula: bound,
            comment,
            hidden,
        };
        if let Err(reason) = self.execute_user_txn(format!("Define {trimmed}"), &[op]) {
            return EngineEvent::Rejected { reason };
        }
        let recalculated = self.run_initial_recalc_slice();
        let sheet_meta = self.sheet_info(self.subscribed_sheet);
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: sheet_meta,
            action_ms: now_ms() - started,
            recalculated,
        }
    }

    fn delete_named_range(&mut self, scope: Option<SheetId>, name: &str) -> EngineEvent {
        let started = now_ms();
        let key = NamedRangeKey::new(scope, name);
        if !self.engine.workbook().named_ranges().contains_key(&key) {
            return EngineEvent::Rejected {
                reason: format!("Named range '{name}' was not found."),
            };
        }
        let op = Op::DeleteNamedRange {
            scope,
            name: name.to_owned(),
        };
        if let Err(reason) = self.execute_user_txn(format!("Delete {name}"), &[op]) {
            return EngineEvent::Rejected { reason };
        }
        let recalculated = self.run_initial_recalc_slice();
        let sheet_meta = self.sheet_info(self.subscribed_sheet);
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: sheet_meta,
            action_ms: now_ms() - started,
            recalculated,
        }
    }

    fn resume_recalc(&mut self) -> EngineEvent {
        self.recalc_frozen = false;
        let wb = self.engine.workbook().clone();
        self.engine = FormulaEngine::from_workbook_recalc_all(wb);
        self.flush_dirty_pages();
        let pages = self.subscribed_pages.clone();
        for p in pages {
            self.enqueue_page(p);
        }
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(self.subscribed_sheet),
            action_ms: 0.0,
            recalculated: u32::try_from(self.engine.workbook().formulas().len()).unwrap_or(0),
        }
    }

    fn continue_recalc(&mut self) -> Option<EngineEvent> {
        if self.recalc_frozen || !self.engine.has_pending_recalc() {
            return None;
        }
        Some(self.step_recalc_slice())
    }

    fn step_recalc_slice(&mut self) -> EngineEvent {
        let slice_start = now_ms();
        let status = self.engine.recalc_with_budget(RECALC_CHECK_INTERVAL, |_| {
            now_ms() - slice_start >= RECALC_SLICE_BUDGET_MS
        });
        let slice_ms = now_ms() - slice_start;
        self.recalc_elapsed_ms += slice_ms;
        self.flush_dirty_pages();

        match status {
            RecalcStatus::InProgress { done, total } => EngineEvent::RecalcProgress { done, total },
            RecalcStatus::Complete { recalculated } => {
                let elapsed_ms = core::mem::take(&mut self.recalc_elapsed_ms);
                let sheet = self.sheet_info(self.subscribed_sheet);
                EngineEvent::RecalcDone {
                    recalculated,
                    elapsed_ms,
                    sheet,
                }
            }
        }
    }
}

// ====================================================== Internal pure helpers

fn check_not_panicked() -> Result<(), JsError> {
    if ENGINE_PANICKED.load(Ordering::SeqCst) {
        return Err(JsError::new(
            "Engine halted after an unrecoverable internal panic; reload to recover.",
        ));
    }
    Ok(())
}

fn f64_to_u64(val: f64) -> Result<u64, JsError> {
    if !val.is_finite() || val < 0.0 || val > 9_007_199_254_740_991.0 {
        return Err(JsError::new(
            "Sequence number must be a safe non-negative integer.",
        ));
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "validated in 0..=2^53-1"
    )]
    Ok(val as u64)
}

fn sanitize_sheet_title(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| match c {
            ':' | '\\' | '/' | '?' | '*' | '[' | ']' => '_',
            other => other,
        })
        .take(31)
        .collect();
    if cleaned.is_empty() {
        String::from("Imported")
    } else {
        cleaned
    }
}

fn cell_label(
    wb: &Workbook<BoundFormula>,
    sheet_id: SheetId,
    row_id: RowId,
    col_id: ColId,
) -> String {
    let Some(sheet) = wb.sheet(sheet_id) else {
        return String::from("cell");
    };
    let (Some(r), Some(c)) = (
        sheet.row_axis().id_to_pos(row_id.0),
        sheet.col_axis().id_to_pos(col_id.0),
    ) else {
        return String::from("cell");
    };
    let mut out = String::new();
    CellAddr {
        col: c,
        row: r,
        abs_col: false,
        abs_row: false,
    }
    .write_a1(&mut out);
    out
}

fn record_inverse_on_snapshot_encoder(enc: &mut SnapshotEncoder, inv: &InverseOp<BoundFormula>) {
    match inv {
        InverseOp::RestoreCells { sheet, cells } => {
            for c in cells {
                let prior = match &c.input {
                    CellInput::Formula { formula, .. } => Some(formula.clone()),
                    CellInput::Empty | CellInput::Scalar(_) => None,
                };
                enc.record_formula_before_edit((*sheet, c.row, c.col), prior);
            }
        }
        InverseOp::RestoreFilledRange {
            sheet,
            rows,
            cols,
            prior_non_empty,
        } => {
            for c in prior_non_empty {
                let prior = match &c.input {
                    CellInput::Formula { formula, .. } => Some(formula.clone()),
                    CellInput::Empty | CellInput::Scalar(_) => None,
                };
                enc.record_formula_before_edit((*sheet, c.row, c.col), prior);
            }
            for r_run in rows {
                for raw_r in r_run.iter() {
                    for c_run in cols {
                        for raw_c in c_run.iter() {
                            enc.record_formula_before_edit(
                                (*sheet, RowId(raw_r), ColId(raw_c)),
                                None,
                            );
                        }
                    }
                }
            }
        }
        InverseOp::RestoreMovedRows { rewritten, .. }
        | InverseOp::RestoreMovedCols { rewritten, .. } => {
            for (k, prior_f) in rewritten {
                enc.record_formula_before_edit(*k, Some(prior_f.clone()));
            }
        }
        InverseOp::RestorePermutedCells { sheet, prior } => {
            for c in prior {
                let p = match &c.input {
                    CellInput::Formula { formula, .. } => Some(formula.clone()),
                    CellInput::Empty | CellInput::Scalar(_) => None,
                };
                enc.record_formula_before_edit((*sheet, c.row, c.col), p);
            }
        }
        InverseOp::RestoreMovedCells {
            prior_cells,
            rewritten_formulas,
            ..
        } => {
            for (sheet, c) in prior_cells {
                let p = match &c.input {
                    CellInput::Formula { formula, .. } => Some(formula.clone()),
                    CellInput::Empty | CellInput::Scalar(_) => None,
                };
                enc.record_formula_before_edit((*sheet, c.row, c.col), p);
            }
            for (k, prior_f) in rewritten_formulas {
                enc.record_formula_before_edit(*k, Some(prior_f.clone()));
            }
        }
        InverseOp::RestoreNamedRange { .. } => {}
        _ => {}
    }
}

fn compute_jump_target(
    wb: &Workbook<BoundFormula>,
    sheet: &Sheet,
    sheet_id: SheetId,
    r0: u32,
    c0: u32,
    dir: JumpDirection,
) -> (u32, u32) {
    let rows = sheet.rows();
    let cols = sheet.cols();
    let is_occ = |r: u32, c: u32| -> bool {
        let (Some(r_id), Some(c_id)) =
            (sheet.row_axis().pos_to_id(r), sheet.col_axis().pos_to_id(c))
        else {
            return false;
        };
        !matches!(sheet.value_by_id(RowId(r_id), ColId(c_id)), Value::Empty)
            || wb.formula(sheet_id, RowId(r_id), ColId(c_id)).is_some()
    };

    // O(1) fast path when jumping vertically along a completely empty column!
    if matches!(dir, JumpDirection::Up | JumpDirection::Down)
        && let Some(c_id) = sheet.col_axis().pos_to_id(c0)
    {
        let col_empty = sheet
            .col_slots()
            .get(c_id)
            .and_then(|slot| sheet.raw_columns().get(slot as usize))
            .is_none_or(|col| col.blocks().is_empty());
        if col_empty && !is_occ(r0, c0) {
            // Check if any formula exists in `c_id` (rare if cached value is Empty).
            let has_any_formula = wb.formulas().col_has_any(sheet_id, ColId(c_id));
            if !has_any_formula {
                return match dir {
                    JumpDirection::Up => (0, c0),
                    JumpDirection::Down => (rows.saturating_sub(1), c0),
                    _ => (r0, c0),
                };
            }
        }
    }

    match dir {
        JumpDirection::Down => {
            if r0.saturating_add(1) >= rows {
                return (r0, c0);
            }
            if is_occ(r0, c0) && is_occ(r0 + 1, c0) {
                let mut r = r0 + 1;
                while r + 1 < rows && is_occ(r + 1, c0) {
                    r += 1;
                }
                (r, c0)
            } else {
                let mut r = r0 + 1;
                while r < rows {
                    if is_occ(r, c0) {
                        return (r, c0);
                    }
                    r += 1;
                }
                (rows - 1, c0)
            }
        }
        JumpDirection::Up => {
            if r0 == 0 {
                return (0, c0);
            }
            if is_occ(r0, c0) && is_occ(r0 - 1, c0) {
                let mut r = r0 - 1;
                while r > 0 && is_occ(r - 1, c0) {
                    r -= 1;
                }
                (r, c0)
            } else {
                let mut r = r0 - 1;
                loop {
                    if is_occ(r, c0) {
                        return (r, c0);
                    }
                    if r == 0 {
                        return (0, c0);
                    }
                    r -= 1;
                }
            }
        }
        JumpDirection::Right => {
            if c0.saturating_add(1) >= cols {
                return (r0, c0);
            }
            if is_occ(r0, c0) && is_occ(r0, c0 + 1) {
                let mut c = c0 + 1;
                while c + 1 < cols && is_occ(r0, c + 1) {
                    c += 1;
                }
                (r0, c)
            } else {
                let mut c = c0 + 1;
                while c < cols {
                    if is_occ(r0, c) {
                        return (r0, c);
                    }
                    c += 1;
                }
                (r0, cols - 1)
            }
        }
        JumpDirection::Left => {
            if c0 == 0 {
                return (r0, 0);
            }
            if is_occ(r0, c0) && is_occ(r0, c0 - 1) {
                let mut c = c0 - 1;
                while c > 0 && is_occ(r0, c - 1) {
                    c -= 1;
                }
                (r0, c)
            } else {
                let mut c = c0 - 1;
                loop {
                    if is_occ(r0, c) {
                        return (r0, c);
                    }
                    if c == 0 {
                        return (r0, 0);
                    }
                    c -= 1;
                }
            }
        }
    }
}

fn format_csv_error(err: CsvError) -> String {
    match err {
        CsvError::InvalidUtf8 => String::from("CSV file is not valid UTF-8 text."),
        CsvError::UnclosedQuote => {
            String::from("CSV file contains an unclosed double-quoted field.")
        }
        CsvError::DimensionsExceeded => {
            String::from("CSV dimensions exceed worksheet limits (2,000,000 rows × 16,384 cols).")
        }
        CsvError::WorkbookLimit(op) => format_op_error(op),
    }
}

fn format_edit_error(err: EditError) -> String {
    match err {
        EditError::Parse(p) => match p {
            ParseError::MissingEquals => String::from("Formula cannot be empty after '='."),
            ParseError::FormulaTooLong => {
                String::from("Formula exceeds maximum length of 8,192 characters.")
            }
            ParseError::NestingTooDeep => {
                String::from("Formula exceeds maximum nesting depth of 64 levels.")
            }
            ParseError::TooManyArguments => {
                String::from("Function call exceeds maximum of 255 arguments.")
            }
            ParseError::Lex(lex) => format!("Invalid token in formula ({lex:?})."),
            ParseError::UnexpectedToken => String::from("Formula syntax error."),
            ParseError::InvalidRange => String::from("Invalid cell or range reference."),
            ParseError::InvalidArrayLiteral => {
                String::from("Invalid array literal (must be rectangular constant literals).")
            }
        },
        EditError::Op(op) => format_op_error(op),
    }
}

fn format_op_error(err: OpError) -> String {
    match err {
        OpError::SheetNotFound => String::from("Target sheet does not exist."),
        OpError::DuplicateSheetId => String::from("Sheet ID already exists in this workbook."),
        OpError::TooManySheets => String::from("Maximum sheet count (256) reached."),
        OpError::InvalidSheetName => {
            String::from("Sheet name must be 1–31 characters and cannot contain : \\ / ? * [ or ].")
        }
        OpError::CannotDeleteLastSheet => String::from("Cannot delete the last visible worksheet."),
        OpError::InvalidSheetOp => String::from("Invalid worksheet operation."),
        OpError::TooManyEdits => String::from("Operation exceeds per-transaction cell limit."),
        OpError::DuplicateCellTarget => String::from("Operation contains duplicate target cells."),
        OpError::TargetNotFound => String::from("Target row or column no longer exists."),
        OpError::Axis(axis_err) => format!("Axis limit or target error ({axis_err:?})."),
        OpError::InvalidDimensionOrFormat => {
            String::from("Dimension, freeze count, or format is out of bounds.")
        }
        OpError::NonFiniteNumber => String::from("Numeric value must be finite."),
        OpError::TextTooLong => String::from("Cell text exceeds 32,767 UTF-16 code units."),
        OpError::FormulaTooLong => {
            String::from("Formula exceeds maximum length of 8,192 characters.")
        }
        OpError::InvalidFormula => String::from("Formula failed validation."),
        OpError::StringPoolExhausted => String::from("String table capacity exhausted."),
        OpError::MemoryLimitExceeded => {
            String::from("Local workbook memory limit (1 GiB) exceeded.")
        }
        OpError::ReadOnly => String::from("This workbook is read-only."),
        OpError::InvalidNamedRange => String::from("Named range identifier or comment is invalid."),
        OpError::TooManyNamedRanges => String::from("Maximum named range count reached."),
        OpError::InvalidPermutation => String::from("Row sort permutation is invalid."),
        OpError::InvalidMoveCells => {
            String::from("Cut-and-paste source and destination ranges are invalid.")
        }
    }
}

pub(crate) fn build_cell_inspected(
    engine: &FormulaEngine,
    sheet_id: SheetId,
    row_id: RowId,
    col_id: ColId,
) -> Option<EngineEvent> {
    let info = engine.inspect_cell(sheet_id, row_id, col_id)?;
    let sheet = engine.workbook().sheet(sheet_id)?;
    let format_code = String::from(sheet.effective_format_by_id(row_id, col_id).code_str());
    let style = WireCellStyle::from(sheet.effective_style_by_id(row_id, col_id));
    let key = (sheet_id, row_id, col_id);
    let anchor_opt = if engine.spills().active_spill(key).is_some() {
        Some(key)
    } else {
        engine.spills().anchor_of_cell(key)
    };
    let is_spill_shadow = anchor_opt.is_some_and(|a| a != key);
    let spill_range = anchor_opt.and_then(|anchor| {
        let entry = engine.spills().active_spill(anchor)?;
        let pr1 = sheet.row_axis().id_to_pos(anchor.1.0)?;
        let c1 = sheet.col_axis().id_to_pos(anchor.2.0)?;
        let pr2 = entry
            .rows
            .last()
            .and_then(|r| sheet.row_axis().id_to_pos(r.0))
            .unwrap_or(pr1);
        let c2 = entry
            .cols
            .last()
            .and_then(|c| sheet.col_axis().id_to_pos(c.0))
            .unwrap_or(c1);
        let d1 = engine.physical_to_display_row(sheet_id, pr1).unwrap_or(pr1);
        let d2 = engine.physical_to_display_row(sheet_id, pr2).unwrap_or(pr2);
        Some(WireCellRect {
            first_row: d1.min(d2),
            last_row: d1.max(d2),
            first_col: c1.min(c2),
            last_col: c1.max(c2),
        })
    });
    Some(EngineEvent::CellInspected {
        sheet: sheet_id.0,
        row_id: row_id.0,
        col_id: col_id.0,
        address: info.address,
        raw: info.raw,
        display: info.display,
        is_formula: info.is_formula,
        error: info.error,
        format_code,
        style,
        spill_range,
        is_spill_shadow,
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "shared SheetInfo DTO constructor for live Engine and isolated HistoryPlayer"
)]
pub(crate) fn build_sheet_info_from_engine(
    engine: &mut FormulaEngine,
    sheet_id: SheetId,
    seed: u32,
    generation_ms: f64,
    can_undo: bool,
    can_redo: bool,
    undo_label: Option<String>,
    redo_label: Option<String>,
    wal_seq: u64,
    recalc_frozen: bool,
) -> (SheetInfo, usize) {
    engine.ensure_sheet_styles_interned(sheet_id);
    let visible_rows = engine.visible_row_count(sheet_id);
    let filter_wire = engine.filter_state(sheet_id).and_then(|(table, proj)| {
        let sheet = engine.workbook().sheet(sheet_id)?;
        let bounds = table.resolve_bounds(sheet)?;
        let mut active_cols: Vec<u32> = table
            .criteria
            .keys()
            .filter_map(|cid| sheet.col_axis().id_to_pos(cid.0))
            .collect();
        active_cols.sort_unstable();
        Some(WireFilterState {
            header_row: bounds.header_row,
            last_row: bounds.last_row,
            first_col: bounds.first_col,
            last_col: bounds.last_col,
            active_cols,
            hidden_row_spans: proj.hidden_spans().to_vec(),
        })
    });
    let wb = engine.workbook();
    let sheets: Vec<SheetTabInfo> = wb
        .live_sheets()
        .map(|s| SheetTabInfo {
            id: s.id().0,
            name: String::from(s.name()),
        })
        .collect();

    let (sheet_name, total_rows, cols, frozen_rows, frozen_cols, custom_rows, custom_cols, styles) =
        wb.sheet(sheet_id).map_or_else(
            || {
                (
                    String::new(),
                    0,
                    0,
                    0,
                    0,
                    Vec::new(),
                    Vec::new(),
                    vec![WireCellStyle::default()],
                )
            },
            |s| {
                let mut cr: Vec<(u32, u16)> = s
                    .row_heights()
                    .iter()
                    .filter_map(|(&r, &px)| {
                        let phys = s.row_axis().id_to_pos(r.0)?;
                        let disp = engine.physical_to_display_row(sheet_id, phys)?;
                        Some((disp, px))
                    })
                    .collect();
                cr.sort_unstable_by_key(|&(pos, _)| pos);

                let mut cc: Vec<(u32, u16)> = s
                    .col_widths()
                    .iter()
                    .filter_map(|(&c, &px)| s.col_axis().id_to_pos(c.0).map(|pos| (pos, px)))
                    .collect();
                cc.sort_unstable_by_key(|&(pos, _)| pos);

                let st: Vec<WireCellStyle> = s
                    .styles()
                    .entries()
                    .iter()
                    .copied()
                    .map(WireCellStyle::from)
                    .collect();
                (
                    String::from(s.name()),
                    s.rows(),
                    s.cols(),
                    s.frozen_rows(),
                    s.frozen_cols(),
                    cr,
                    cc,
                    st,
                )
            },
        );
    let styles_len = styles.len();
    let formula_count = u32::try_from(wb.formulas().len()).unwrap_or(0);
    let rows = if filter_wire.is_some() {
        visible_rows
    } else {
        total_rows
    };
    let info = SheetInfo {
        sheet: sheet_id.0,
        sheet_name,
        sheets,
        rows,
        total_rows,
        cols,
        filter: filter_wire,
        frozen_rows,
        frozen_cols,
        custom_rows,
        custom_cols,
        styles,
        formula_count,
        seed,
        generation_ms,
        can_undo,
        can_redo,
        undo_label,
        redo_label,
        wal_seq,
        recalc_frozen,
    };
    (info, styles_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_state_machine_handles_sample_edit_inspect_insert_delete_and_dirty_pages() {
        let mut eng = Engine::new();
        let ready = eng
            .handle_command(EngineCommand::OpenSample {
                seed: 20261117,
                rows: 500,
            })
            .unwrap();
        let EngineEvent::Ready { sheet, .. } = ready else {
            panic!("expected Ready");
        };
        assert_eq!(sheet.rows, 500);
        assert_eq!(sheet.cols, 22);
        assert_eq!(sheet.formula_count, 500);

        // Seed stub for unmodified sample should be < 200 bytes (`14-m1b.md` 7.1).
        let stub = eng.encode_seed_stub().unwrap().expect("seed stub");
        assert!(stub.len() < 200, "stub.len()={}", stub.len());

        // Subscribe to top-left 64 rows x 22 cols (2 pages: (0,0) and (0,1)).
        assert!(
            eng.handle_command(EngineCommand::Subscribe {
                sheet: 0,
                epoch: 1,
                first_row: 0,
                row_count: 64,
                first_col: 0,
                col_count: 22,
            })
            .is_none()
        );
        let p0 = eng.take_page().expect("page (0,0)");
        let p1 = eng.take_page().expect("page (0,1)");
        assert!(eng.take_page().is_none());
        assert!(!p0.is_empty() && !p1.is_empty());

        // Inspect V2 (`row_id = 1, col_id = 21`).
        let inspected = eng
            .handle_command(EngineCommand::InspectCell {
                sheet: 0,
                row_id: 1,
                col_id: 21,
            })
            .unwrap();
        let EngineEvent::CellInspected {
            address,
            raw,
            is_formula,
            format_code,
            ..
        } = inspected
        else {
            panic!("expected CellInspected");
        };
        assert_eq!(address, "V2");
        assert_eq!(raw, "=SUM(M2:M500)");
        assert!(is_formula);
        assert_eq!(format_code, "0.00");

        // Edit V1 (`row_id = 0, col_id = 21`) to `5%` -> triggers recalculation, WAL frame & undo!
        let mut ev = eng
            .handle_command(EngineCommand::EditCell {
                sheet: 0,
                row_id: 0,
                col_id: 21,
                raw: String::from("5%"),
            })
            .unwrap();
        while matches!(ev, EngineEvent::RecalcProgress { .. }) {
            ev = eng.handle_command(EngineCommand::ContinueRecalc).unwrap();
        }
        let EngineEvent::RecalcDone {
            recalculated,
            sheet: post_edit_sheet,
            ..
        } = ev
        else {
            panic!("expected RecalcDone, got {ev:?}");
        };
        assert_eq!(recalculated, 500);
        assert!(post_edit_sheet.can_undo);
        assert_eq!(post_edit_sheet.undo_label.as_deref(), Some("Edit V1"));
        assert_eq!(post_edit_sheet.wal_seq, 1);
        let wal_frame_1 = eng.take_pending_wal_frame().expect("wal frame 1");
        assert!(!wal_frame_1.is_empty());

        while eng.take_page().is_some() {}

        // Insert 1 row at the very top (`after_row_id: None`) -> rows becomes 501, V2 becomes V3!
        let ins = eng
            .handle_command(EngineCommand::InsertRows {
                sheet: 0,
                after_row_id: None,
                count: 1,
            })
            .unwrap();
        let EngineEvent::SheetChanged { sheet: s2, .. } = ins else {
            panic!("expected SheetChanged");
        };
        assert_eq!(s2.rows, 501);
        assert_eq!(s2.wal_seq, 2);
        let wal_frame_2 = eng.take_pending_wal_frame().expect("wal frame 2");
        while eng.take_page().is_some() {}

        // Replay `stub + wal_frame_1 + wal_frame_2` into a fresh Engine and verify bit-identical digest!
        let mut full_wal = encode_wal_init_v3(0).unwrap();
        full_wal.extend_from_slice(&wal_frame_1);
        full_wal.extend_from_slice(&wal_frame_2);

        let expected_digest = eng.engine.workbook().logical_digest();
        let mut reloaded = Engine::new();
        let decoded = decode_snapshot_any(&stub).unwrap();
        reloaded.engine = decoded.engine;
        let scan = scan_wal_any_v3(&full_wal, 0, 2).unwrap();
        replay_wal_records_v3(&mut reloaded.engine, &scan.records, 0, true).unwrap();
        assert_eq!(reloaded.engine.workbook().logical_digest(), expected_digest);

        // Undo InsertRows -> rows returns to 500!
        let undone = eng.handle_command(EngineCommand::Undo).unwrap();
        let EngineEvent::SheetChanged { sheet: s_undo, .. } = undone else {
            panic!("expected SheetChanged on Undo");
        };
        assert_eq!(s_undo.rows, 500);
        assert!(s_undo.can_redo);

        // Malformed formula is rejected without mutating state.
        let rej = eng
            .handle_command(EngineCommand::EditCell {
                sheet: 0,
                row_id: 0,
                col_id: 0,
                raw: String::from("=SUM(1,"),
            })
            .unwrap();
        assert!(matches!(rej, EngineEvent::Rejected { .. }));
    }

    #[test]
    fn freeze_panes_pin_pages_and_jump_edge_follows_excel_rules() {
        let mut eng = Engine::new();
        let _ = eng.handle_command(EngineCommand::OpenEmpty).unwrap();
        // Set A1..A3 = 1, 2, 3 and A10 = 99
        for (r, v) in [(0_u64, "1"), (1, "2"), (2, "3"), (9, "99")] {
            let _ = eng.handle_command(EngineCommand::EditCell {
                sheet: 0,
                row_id: r,
                col_id: 0,
                raw: String::from(v),
            });
        }
        // Jump Down from A1 -> A3 (end of contiguous block) -> A10 (next non-empty) -> bottom edge!
        let j1 = eng
            .handle_command(EngineCommand::JumpEdge {
                sheet: 0,
                row: 0,
                col: 0,
                dir: JumpDirection::Down,
                select: false,
            })
            .unwrap();
        assert_eq!(
            j1,
            EngineEvent::EdgeJumped {
                sheet: 0,
                row: 2,
                col: 0,
                select: false
            }
        );
        let j2 = eng
            .handle_command(EngineCommand::JumpEdge {
                sheet: 0,
                row: 2,
                col: 0,
                dir: JumpDirection::Down,
                select: false,
            })
            .unwrap();
        assert_eq!(
            j2,
            EngineEvent::EdgeJumped {
                sheet: 0,
                row: 9,
                col: 0,
                select: false
            }
        );

        // Freeze top 1 row, then subscribe deep down at row 10,000 -> page (0,0) stays pinned!
        let _ = eng
            .handle_command(EngineCommand::SetFreeze {
                sheet: 0,
                frozen_rows: 1,
                frozen_cols: 0,
            })
            .unwrap();
        eng.handle_command(EngineCommand::Subscribe {
            sheet: 0,
            epoch: 2,
            first_row: 10_000,
            row_count: 32,
            first_col: 0,
            col_count: 8,
        });
        assert!(eng.subscribed_set.contains(&Page { row: 0, col: 0 }));
    }

    #[test]
    fn worker_step4_filter_sort_search_named_ranges_and_spill_inspect_work() {
        let mut eng = Engine::new();
        let _ = eng.handle_command(EngineCommand::OpenEmpty).unwrap();

        // Populate A1:B4 with header + 3 rows, and D1 = "=SEQUENCE(3,1)" + E1 = "=SUBTOTAL(109, B2:B4)".
        for (r, label, val) in [
            (0, "Region", "Amt"),
            (1, "West", "30"),
            (2, "East", "10"),
            (3, "West", "20"),
        ] {
            let _ = eng.handle_command(EngineCommand::EditCell {
                sheet: 0,
                row_id: r,
                col_id: 0,
                raw: String::from(label),
            });
            let _ = eng.handle_command(EngineCommand::EditCell {
                sheet: 0,
                row_id: r,
                col_id: 1,
                raw: String::from(val),
            });
        }
        let _ = eng.handle_command(EngineCommand::EditCell {
            sheet: 0,
            row_id: 0,
            col_id: 3,
            raw: String::from("=SEQUENCE(3,1)"),
        });
        let _ = eng.handle_command(EngineCommand::EditCell {
            sheet: 0,
            row_id: 0,
            col_id: 4,
            raw: String::from("=SUBTOTAL(109, B2:B4)"),
        });

        // Define named range `MyAmt` = `=$B$2:$B$4` and list it cleanly.
        let def_ev = eng
            .handle_command(EngineCommand::DefineNamedRange {
                scope: None,
                name: String::from("MyAmt"),
                formula: String::from("=B2:B4"),
                comment: Some(String::from("Amounts")),
                hidden: Some(false),
            })
            .unwrap();
        assert!(matches!(def_ev, EngineEvent::SheetChanged { .. }));
        let EngineEvent::NamedRangesListed { items } =
            eng.handle_command(EngineCommand::ListNamedRanges).unwrap()
        else {
            panic!("expected NamedRangesListed");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "MyAmt");

        // Sort A1:B4 ascending by column B (Amt: 10, 20, 30) with auto header detection!
        let sort_ev = eng
            .handle_command(EngineCommand::SortRange {
                sheet: 0,
                first_row: 0,
                last_row: 3,
                first_col: 0,
                last_col: 1,
                has_header: None,
                keys: vec![WireSortKey {
                    col_offset: 1,
                    ascending: true,
                    case_sensitive: false,
                }],
            })
            .unwrap();
        let EngineEvent::SheetChanged {
            sheet: s_after_sort,
            ..
        } = sort_ev
        else {
            panic!("expected SheetChanged");
        };
        assert!(s_after_sort.can_undo);
        assert_eq!(
            eng.engine.workbook().sheet(SheetId(0)).unwrap().value(1, 1),
            reticula_core::value::Value::number(10.0)
        );

        // Apply AutoFilter on A1:B4 and filter col 0 == "West" (hides East/row 1 -> Amt=10).
        let _ = eng
            .handle_command(EngineCommand::SetFilterTable {
                sheet: 0,
                header_row: 0,
                last_row: 3,
                first_col: 0,
                last_col: 1,
            })
            .unwrap();
        let EngineEvent::FilterValues { values, .. } = eng
            .handle_command(EngineCommand::GetFilterValues { sheet: 0, col: 0 })
            .unwrap()
        else {
            panic!("expected FilterValues");
        };
        assert_eq!(values.len(), 2);

        let f_ev = eng
            .handle_command(EngineCommand::SetColumnFilter {
                sheet: 0,
                col: 0,
                criterion: Some(WireColFilterCriterion::Values {
                    allowed: vec![String::from("West")],
                    include_blanks: false,
                }),
            })
            .unwrap();
        let EngineEvent::SheetChanged { sheet: f_sheet, .. } = f_ev else {
            panic!("expected SheetChanged");
        };
        assert_eq!(f_sheet.rows, f_sheet.total_rows - 1);
        assert!(f_sheet.filter.is_some());
        assert_eq!(
            eng.engine.workbook().sheet(SheetId(0)).unwrap().value(0, 4),
            reticula_core::value::Value::number(50.0)
        );

        // Inspect spill shadow cell D2 (`row_id = 1, col_id = 3`): reports spill_range + is_spill_shadow!
        let EngineEvent::CellInspected {
            spill_range,
            is_spill_shadow,
            ..
        } = eng
            .handle_command(EngineCommand::InspectCell {
                sheet: 0,
                row_id: 1,
                col_id: 3,
            })
            .unwrap()
        else {
            panic!("expected CellInspected");
        };
        assert!(is_spill_shadow);
        assert!(spill_range.is_some());

        // Clear filter restores visible row count & SUBTOTAL back to 60.
        let cleared = eng
            .handle_command(EngineCommand::ClearFilter { sheet: 0 })
            .unwrap();
        let EngineEvent::SheetChanged { sheet: c_sheet, .. } = cleared else {
            panic!("expected SheetChanged");
        };
        assert_eq!(c_sheet.rows, c_sheet.total_rows);
        assert!(c_sheet.filter.is_none());
        assert_eq!(
            eng.engine.workbook().sheet(SheetId(0)).unwrap().value(0, 4),
            reticula_core::value::Value::number(60.0)
        );
    }
}
