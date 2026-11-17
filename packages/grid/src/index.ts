// The grid: a canvas renderer with its own scrolling, fed with viewport pages.
// It knows nothing about the engine or about any UI framework; its host wires
// it to both. Its styles are in `@reticula/grid/grid.css`.
export {
  type FrameReport,
  Grid,
  type GridOptions,
  type GridStats,
  type Selection,
  type Subscription,
} from "./grid.ts";
export type { Metrics, Scroll, SheetSize, Span } from "./layout.ts";
