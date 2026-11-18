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
  type GridOptions,
  type GridStats,
  type IdRun,
  type Selection,
  type Subscription,
} from "./grid.ts";
export {
  type Box,
  columnName,
  type Metrics,
  type Scroll,
  type SheetSize,
  type Span,
} from "./layout.ts";
