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
  coalesceIdRuns,
  type FrameReport,
  Grid,
  type GridContextTarget,
  type GridOptions,
  type GridStats,
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
export type {
  FormulaReferenceOverlay,
  GridCellStyle,
} from "./renderer.ts";
export { REF_COLORS_DARK, REF_COLORS_LIGHT } from "./theme.ts";
