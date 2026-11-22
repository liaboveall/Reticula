//! XLSX import, export, and streaming original-workbook patching (`17-m3a.md` §7).
//!
//! This crate intentionally lives outside the deterministic kernel (`core`,
//! `formula`, `wire`, `sync`) so the browser workspace bundle stays compact while
//! XLSX support loads on demand via `reticula-xlsx-wasm`.

mod export;
mod import;
mod patch;
mod xlfn;
mod xml_scan;

use core::fmt;

use reticula_formula::engine::FormulaEngine;
use reticula_wire::snapshot::{decode_snapshot_workbook_only, encode_snapshot_v1};
use serde::{Deserialize, Serialize};

pub use export::export_xlsx_with_options;
pub use import::import_xlsx;
pub use patch::patch_xlsx;

/// Maximum accepted raw `.xlsx` ZIP container size (`64 MiB`).
pub const MAX_XLSX_INPUT_BYTES: usize = 64 * 1024 * 1024;
/// Maximum cumulative uncompressed XML payload accepted during import (`256 MiB`).
pub const MAX_XLSX_UNCOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
/// Excel worksheet row limit (`1,048,576`).
pub const EXCEL_MAX_ROWS: u32 = 1_048_576;
/// Excel worksheet column limit (`16,384`, `XFD`).
pub const EXCEL_MAX_COLS: u32 = 16_384;

/// Errors produced when importing, exporting, or patching `.xlsx` workbooks.
#[derive(Debug)]
pub enum XlsxError {
    /// Compressed archive exceeded [`MAX_XLSX_INPUT_BYTES`].
    InputTooLarge { bytes: usize },
    /// Decompressed XML streams exceeded [`MAX_XLSX_UNCOMPRESSED_BYTES`].
    UncompressedTooLarge { bytes: u64 },
    /// Malformed or truncated ZIP container.
    InvalidArchive(String),
    /// Malformed OOXML XML document or missing required entry.
    Xml(String),
    /// Error reported by `calamine` while reading workbook sheets/values.
    Calamine(String),
    /// Error reported by `rust_xlsxwriter` while constructing `.xlsx`.
    Write(String),
    /// Error encoding or decoding an `RTCLSNAP` snapshot buffer.
    Snapshot(String),
    /// Workbook construction or axis limit violation in `reticula-core`.
    Workbook(String),
    /// Requested worksheet for patching does not exist in target workbook.
    SheetNotFound(String),
}

impl fmt::Display for XlsxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { bytes } => write!(
                f,
                "XLSX input ({bytes} bytes) exceeds 64 MiB limit ({MAX_XLSX_INPUT_BYTES} bytes)"
            ),
            Self::UncompressedTooLarge { bytes } => write!(
                f,
                "XLSX decompressed XML ({bytes} bytes) exceeds 256 MiB limit ({MAX_XLSX_UNCOMPRESSED_BYTES} bytes)"
            ),
            Self::InvalidArchive(msg) => write!(f, "invalid XLSX archive: {msg}"),
            Self::Xml(msg) => write!(f, "invalid OOXML XML: {msg}"),
            Self::Calamine(msg) => write!(f, "XLSX read error: {msg}"),
            Self::Write(msg) => write!(f, "XLSX write error: {msg}"),
            Self::Snapshot(msg) => write!(f, "snapshot codec error: {msg}"),
            Self::Workbook(msg) => write!(f, "workbook construction error: {msg}"),
            Self::SheetNotFound(name) => write!(f, "worksheet not found for patch: {name}"),
        }
    }
}

impl std::error::Error for XlsxError {}

/// Classification of unsupported OOXML features surfaced during import.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UnsupportedKind {
    /// Merged cell ranges (`<mergeCells>`).
    MergedCells,
    /// Embedded charts (`xl/charts/*.xml`).
    Chart,
    /// Drawings, shapes, or embedded images (`xl/drawings/*.xml`, `xl/media/*`).
    DrawingOrImage,
    /// VBA macro project (`xl/vbaProject.bin`).
    VbaMacro,
    /// Conditional formatting rules (`<conditionalFormatting>`).
    ConditionalFormatting,
    /// Pivot tables or pivot caches (`xl/pivotTables/*.xml`).
    PivotTable,
    /// Cell comments or threaded comments.
    Comment,
    /// External workbook links or data connections.
    ExternalConnection,
}

/// Summary of an unsupported workbook feature found in a specific sheet or package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XlsxUnsupportedFeature {
    /// Feature category.
    pub kind: UnsupportedKind,
    /// Owning worksheet name when sheet-scoped, or `None` when package-wide.
    pub sheet: Option<String>,
    /// Count of affected objects or rules.
    pub count: u32,
    /// Human-readable diagnostic detail.
    pub detail: String,
}

/// Non-fatal inventory returned alongside an imported workbook.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XlsxImportReport {
    /// Number of imported worksheets.
    pub sheet_count: u32,
    /// Number of populated static cells.
    pub cell_count: u32,
    /// Total number of formulas imported (executable + preserved).
    pub formula_count: u32,
    /// Formulas preserved as `Opaque` or `PreservedCached` due to external/unknown syntax.
    pub preserved_formula_count: u32,
    /// Cells skipped because coordinates exceeded Excel sheet bounds.
    pub truncated_cells: u32,
    /// Unsupported OOXML features detected during import.
    pub unsupported: Vec<XlsxUnsupportedFeature>,
}

/// Result of [`import_xlsx`].
#[derive(Debug)]
pub struct XlsxImportResult {
    /// Recalculated formula engine ready for editing or snapshotting.
    pub engine: FormulaEngine,
    /// Structured import telemetry and unsupported feature report.
    pub report: XlsxImportReport,
}

/// Configuration knobs for `.xlsx` export.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportOptions {
    /// Omit `<v>` cached values inside formula cells so downstream evaluators
    /// (Excel / LibreOffice Calc) recalculate from scratch on load.
    pub omit_cached_values: bool,
    /// Force `<calcPr fullCalcOnLoad="1"/>` in `xl/workbook.xml`.
    pub full_calc_on_load: bool,
}

/// Non-fatal warning emitted during export or patching.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XlsxExportWarning {
    /// Stable machine-readable warning code.
    pub code: String,
    /// Human-readable description.
    pub message: String,
    /// Count of truncated cells or affected items.
    pub count: u32,
}

/// Output bytes and diagnostics from `.xlsx` generation or patching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XlsxExportResult {
    /// Complete `.xlsx` ZIP payload.
    pub bytes: Vec<u8>,
    /// Export warnings (for example Excel E7 coordinate truncation).
    pub warnings: Vec<XlsxExportWarning>,
}

/// 0-based worksheet coordinate identifying a cell modified since original `.xlsx` import.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedCell {
    /// Worksheet display name.
    pub sheet_name: String,
    /// 0-based live row position.
    pub row: u32,
    /// 0-based live column position.
    pub col: u32,
}

/// Imports an `.xlsx` byte slice and encodes the resulting workbook as an `RTCLSNAP` snapshot.
pub fn import_xlsx_to_snapshot(bytes: &[u8]) -> Result<(Vec<u8>, XlsxImportReport), XlsxError> {
    let (wb, report) = import::import_xlsx_workbook(bytes)?;
    let snap = encode_snapshot_v1(&wb, &[]).map_err(|e| XlsxError::Snapshot(format!("{e:?}")))?;
    Ok((snap, report))
}

/// Exports `engine` using default OOXML options via `rust_xlsxwriter`.
pub fn export_xlsx(engine: &FormulaEngine) -> Result<XlsxExportResult, XlsxError> {
    export_xlsx_with_options(engine, ExportOptions::default())
}

/// Decodes an `RTCLSNAP` snapshot buffer and exports it as `.xlsx`.
pub fn export_snapshot_to_xlsx(snapshot_bytes: &[u8]) -> Result<XlsxExportResult, XlsxError> {
    let wb = decode_snapshot_workbook_only(snapshot_bytes)
        .map_err(|e| XlsxError::Snapshot(format!("{e:?}")))?;
    export::export_workbook_with_options(&wb, None, ExportOptions::default())
}

/// Decodes an `RTCLSNAP` snapshot buffer and applies a streaming patch over `orig_xlsx`.
pub fn patch_snapshot_to_xlsx(
    orig_xlsx: &[u8],
    snapshot_bytes: &[u8],
    changed_cells: &[ChangedCell],
) -> Result<XlsxExportResult, XlsxError> {
    let wb = decode_snapshot_workbook_only(snapshot_bytes)
        .map_err(|e| XlsxError::Snapshot(format!("{e:?}")))?;
    patch::patch_workbook_xlsx(orig_xlsx, &wb, None, changed_cells)
}
