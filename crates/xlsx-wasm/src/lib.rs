//! On-demand WebAssembly bindings for `reticula-xlsx` (`17-m3a.md` §7).

use reticula_xlsx::{
    ChangedCell, UnsupportedKind, XlsxExportWarning, XlsxImportReport, XlsxUnsupportedFeature,
    export_snapshot_to_xlsx, import_xlsx_to_snapshot, patch_snapshot_to_xlsx,
};
use serde::{Deserialize, Serialize};
use tsify::{Ts, Tsify};
use wasm_bindgen::prelude::*;

/// Category of an unsupported OOXML feature encountered during `.xlsx` import.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum WireUnsupportedKind {
    MergedCells,
    Chart,
    DrawingOrImage,
    VbaMacro,
    ConditionalFormatting,
    PivotTable,
    Comment,
    ExternalConnection,
}

impl From<UnsupportedKind> for WireUnsupportedKind {
    fn from(value: UnsupportedKind) -> Self {
        match value {
            UnsupportedKind::MergedCells => Self::MergedCells,
            UnsupportedKind::Chart => Self::Chart,
            UnsupportedKind::DrawingOrImage => Self::DrawingOrImage,
            UnsupportedKind::VbaMacro => Self::VbaMacro,
            UnsupportedKind::ConditionalFormatting => Self::ConditionalFormatting,
            UnsupportedKind::PivotTable => Self::PivotTable,
            UnsupportedKind::Comment => Self::Comment,
            UnsupportedKind::ExternalConnection => Self::ExternalConnection,
        }
    }
}

/// Summary of an unsupported workbook feature found during `.xlsx` import.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireXlsxUnsupportedFeature {
    pub kind: WireUnsupportedKind,
    pub sheet: Option<String>,
    pub count: u32,
    pub detail: String,
}

impl From<XlsxUnsupportedFeature> for WireXlsxUnsupportedFeature {
    fn from(value: XlsxUnsupportedFeature) -> Self {
        Self {
            kind: value.kind.into(),
            sheet: value.sheet,
            count: value.count,
            detail: value.detail,
        }
    }
}

/// Structured import report exposed to TypeScript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireXlsxImportReport {
    pub sheet_count: u32,
    pub cell_count: u32,
    pub formula_count: u32,
    pub preserved_formula_count: u32,
    pub truncated_cells: u32,
    pub unsupported: Vec<WireXlsxUnsupportedFeature>,
}

impl From<XlsxImportReport> for WireXlsxImportReport {
    fn from(value: XlsxImportReport) -> Self {
        Self {
            sheet_count: value.sheet_count,
            cell_count: value.cell_count,
            formula_count: value.formula_count,
            preserved_formula_count: value.preserved_formula_count,
            truncated_cells: value.truncated_cells,
            unsupported: value.unsupported.into_iter().map(Into::into).collect(),
        }
    }
}

/// Non-fatal warning produced while exporting or patching `.xlsx`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireXlsxExportWarning {
    pub code: String,
    pub message: String,
    pub count: u32,
}

impl From<XlsxExportWarning> for WireXlsxExportWarning {
    fn from(value: XlsxExportWarning) -> Self {
        Self {
            code: value.code,
            message: value.message,
            count: value.count,
        }
    }
}

/// Worksheet cell coordinate modified relative to an original `.xlsx` buffer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireChangedCell {
    pub sheet_name: String,
    pub row: u32,
    pub col: u32,
}

impl From<WireChangedCell> for ChangedCell {
    fn from(value: WireChangedCell) -> Self {
        Self {
            sheet_name: value.sheet_name,
            row: value.row,
            col: value.col,
        }
    }
}

/// Transparent array wrapper exposing `WireXlsxExportWarning[]` cleanly via `tsify`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(transparent)]
pub struct WireXlsxWarnings(pub Vec<WireXlsxExportWarning>);

/// Transparent array wrapper accepting `WireChangedCell[]` cleanly via `tsify`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(transparent)]
pub struct WireChangedCells(pub Vec<WireChangedCell>);

/// Decoded `RTCLSNAP` bytes + import telemetry returned by [`import_xlsx`].
#[wasm_bindgen]
pub struct XlsxImportBundle {
    snapshot: Option<Vec<u8>>,
    report: WireXlsxImportReport,
}

#[wasm_bindgen]
impl XlsxImportBundle {
    /// Transfers ownership of the encoded `RTCLSNAP` buffer to JavaScript.
    #[wasm_bindgen(js_name = takeSnapshot)]
    pub fn take_snapshot(&mut self) -> Result<Vec<u8>, JsError> {
        self.snapshot
            .take()
            .ok_or_else(|| JsError::new("snapshot bytes were already consumed"))
    }

    /// Returns the structured import telemetry and unsupported feature list.
    #[wasm_bindgen]
    pub fn report(&self) -> Result<Ts<WireXlsxImportReport>, JsError> {
        Ts::from_rust(&self.report).map_err(Into::into)
    }
}

/// Encoded `.xlsx` archive bytes + warnings returned by [`export_xlsx_snapshot`]
/// and [`patch_xlsx_snapshot`].
#[wasm_bindgen]
pub struct XlsxExportBundle {
    bytes: Option<Vec<u8>>,
    warnings: Vec<WireXlsxExportWarning>,
}

#[wasm_bindgen]
impl XlsxExportBundle {
    /// Transfers ownership of the encoded `.xlsx` archive to JavaScript.
    #[wasm_bindgen(js_name = takeBytes)]
    pub fn take_bytes(&mut self) -> Result<Vec<u8>, JsError> {
        self.bytes
            .take()
            .ok_or_else(|| JsError::new("xlsx bytes were already consumed"))
    }

    /// Returns any non-fatal export warnings (such as Excel row/column truncation).
    #[wasm_bindgen]
    pub fn warnings(&self) -> Result<Ts<WireXlsxWarnings>, JsError> {
        Ts::from_rust(&WireXlsxWarnings(self.warnings.clone())).map_err(Into::into)
    }
}

/// Imports an `.xlsx` byte slice and returns an [`XlsxImportBundle`] containing
/// an `RTCLSNAP` snapshot plus structured unsupported-feature diagnostics.
#[wasm_bindgen(js_name = importXlsx)]
pub fn import_xlsx(bytes: &[u8]) -> Result<XlsxImportBundle, JsError> {
    let (snapshot, report) =
        import_xlsx_to_snapshot(bytes).map_err(|e| JsError::new(&e.to_string()))?;
    Ok(XlsxImportBundle {
        snapshot: Some(snapshot),
        report: report.into(),
    })
}

/// Decodes an `RTCLSNAP` snapshot and exports it as a fresh `.xlsx` archive.
#[wasm_bindgen(js_name = exportXlsxSnapshot)]
pub fn export_xlsx_snapshot(snapshot_bytes: &[u8]) -> Result<XlsxExportBundle, JsError> {
    let result =
        export_snapshot_to_xlsx(snapshot_bytes).map_err(|e| JsError::new(&e.to_string()))?;
    Ok(XlsxExportBundle {
        bytes: Some(result.bytes),
        warnings: result.warnings.into_iter().map(Into::into).collect(),
    })
}

/// Decodes an `RTCLSNAP` snapshot and streams a surgical patch over `orig_xlsx`.
#[expect(
    clippy::needless_pass_by_value,
    reason = "wasm_bindgen ABI takes Ts<T> by value"
)]
#[wasm_bindgen(js_name = patchXlsxSnapshot)]
pub fn patch_xlsx_snapshot(
    orig_xlsx: &[u8],
    snapshot_bytes: &[u8],
    changed_cells: Ts<WireChangedCells>,
) -> Result<XlsxExportBundle, JsError> {
    let parsed = changed_cells.to_rust()?;
    let converted: Vec<ChangedCell> = parsed.0.into_iter().map(Into::into).collect();
    let result = patch_snapshot_to_xlsx(orig_xlsx, snapshot_bytes, &converted)
        .map_err(|e| JsError::new(&e.to_string()))?;
    Ok(XlsxExportBundle {
        bytes: Some(result.bytes),
        warnings: result.warnings.into_iter().map(Into::into).collect(),
    })
}
