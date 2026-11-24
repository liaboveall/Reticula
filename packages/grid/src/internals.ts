// The grid's parts, for code that measures them one at a time. An application
// uses the package's main entry instead.
export { PageCache } from "./cache.ts";
export * from "./layout.ts";
export {
  decodePage,
  isBool,
  isError,
  isFormula,
  isNumber,
  isTruncated,
  type Page,
} from "./page.ts";
export { Canvas2dRenderer } from "./render/canvas2d.ts";
export type {
  DiffHighlightKind,
  DiffHighlightMap,
  FormulaReferenceOverlay,
  Frame,
  FrameCounts,
  GridCellStyle,
  RemotePresenceOverlay,
  ReorderGuide,
} from "./render/frame.ts";
export { BAND_ROWS } from "./render/page-layout.ts";
export { type DrawWebGpu, drawCanvas2d, masksOf } from "./render/testing.ts";
export { TextMeasurer } from "./render/text-measure.ts";
export type {
  FrameMasks,
  GridRenderer,
  Pixels,
  RendererCounters,
  RendererKind,
} from "./render/types.ts";
export { acquire } from "./render/webgpu/device.ts";
export { WebGpuRenderer } from "./render/webgpu/renderer.ts";
export { drawWebGpu } from "./render/webgpu/testing.ts";
export { readTheme, type Theme } from "./theme.ts";
