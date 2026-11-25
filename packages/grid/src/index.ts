// The grid: a canvas renderer with its own scrolling, fed with viewport pages.
// It knows nothing about the engine or about any UI framework; its host wires
// it to both. Its styles are in `@reticula/grid/grid.css`.
export {
  CellEditor,
  type CellEditorOptions,
  type CellPosition,
  type EditorEvent,
} from "./editor.ts";
export {
  type CellRect,
  FILTER_BUTTON_SIZE,
  type FilterButtonRect,
  type FilterDisplayState,
  FilterRowMap,
  filterButtonBox,
} from "./filter.ts";
export {
  FormulaBarEditor,
  type FormulaBarEditorOptions,
  formulaDecorationExtension,
  setFormulaAnalysisEffect,
} from "./formula-input.ts";
export {
  coalesceIdRuns,
  type FrameReport,
  Grid,
  type GridContextTarget,
  type GridOptions,
  type GridStats,
  type GridView,
  type IdRun,
  type Selection,
  type SelectionScope,
  type SheetConfig,
  type Subscription,
} from "./grid.ts";
export {
  type Box,
  columnName,
  type HeaderHit,
  headerHitAt,
  type Metrics,
  type Scroll,
  type SheetLayout,
  type SheetSize,
  type Span,
  SparseAxis,
} from "./layout.ts";
export { IS_MAC, type JumpDirection } from "./navigation.ts";
export type {
  DiffHighlightKind,
  DiffHighlightMap,
  FormulaReferenceOverlay,
  GridCellStyle,
  RemotePresenceOverlay,
} from "./render/frame.ts";
export type { RendererPreference, RendererReason } from "./render/upgrade.ts";
export type { GridStrings } from "./strings.ts";
export {
  DARK_PALETTE_FILL,
  DARK_PALETTE_TEXT,
  LIGHT_PALETTE_FILL,
  LIGHT_PALETTE_TEXT,
  REF_COLORS_DARK,
  REF_COLORS_LIGHT,
} from "./theme.ts";
export { clampZoom, MAX_ZOOM, MIN_ZOOM, nextZoom, ZOOM_STEPS } from "./zoom.ts";
