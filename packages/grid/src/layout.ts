// The arithmetic of the grid: sparse custom row/column metrics (`O(log k)`,
// `O(1)` when uniform), frozen-pane quadrants, which cells are on screen, what
// a pixel points at, and how far one can scroll. Everything here is a pure
// function of numbers, so it is tested without a browser.

import {
  MAX_SUBSCRIBED_PAGE_COLS,
  MAX_SUBSCRIBED_PAGE_ROWS,
  MAX_SUBSCRIBED_PAGES,
  MAX_VISIBLE_PAGES_PER_AXIS,
  PAGE_COLS,
  PAGE_ROWS,
} from "@reticula/protocol/viewport";

/**
 * Sizes in CSS pixels: the ones the workbook has, which are what operations
 * carry, or the ones they come to on the screen (`viewMetrics`).
 */
export interface Metrics {
  readonly rowHeight: number;
  readonly colWidth: number;
  /** Height of the strip of column names along the top. */
  readonly headerHeight: number;
  /** Width of the strip of row numbers along the left. */
  readonly headerWidth: number;
  /** Thickness of the scrollbars along the right and the bottom. */
  readonly scrollbarSize: number;
}

/** The chosen sizes are whole device pixels at every quarter-step zoom. */
export const DEFAULT_METRICS: Metrics = {
  rowHeight: 24,
  colWidth: 112,
  headerHeight: 24,
  headerWidth: 64,
  scrollbarSize: 12,
};

/** Whole device pixels for a size in CSS pixels. Only what has no size at all gets none. */
function deviceUnits(size: number, deviceScale: number): number {
  return size > 0 ? Math.max(1, Math.round(size * deviceScale)) : 0;
}

/** How much the headers are enlarged: less than the cells, as they have to stay legible. */
export function headerZoom(zoom: number): number {
  return Math.min(1.5, Math.max(0.75, zoom));
}

/**
 * The sizes on the screen. All of them are whole device pixels: the same rows
 * would otherwise come out higher or lower depending on where they are, and
 * sums of them would lead to other rows than the ones that are drawn.
 */
export function viewMetrics(base: Metrics, zoom: number, deviceScale: number): Metrics {
  const sized = (size: number, factor: number) =>
    deviceUnits(size * factor, deviceScale) / deviceScale;
  const header = headerZoom(zoom);
  return {
    rowHeight: sized(base.rowHeight, zoom),
    colWidth: sized(base.colWidth, zoom),
    headerHeight: sized(base.headerHeight, header),
    headerWidth: sized(base.headerWidth, header),
    scrollbarSize: sized(base.scrollbarSize, 1),
  };
}

export interface SheetSize {
  readonly rows: number;
  readonly cols: number;
}

export interface Size {
  readonly width: number;
  readonly height: number;
}

export interface Scroll {
  readonly top: number;
  readonly left: number;
}

/** A rectangle of cells or of pages: rows and columns, each half-open. */
export interface Span {
  readonly firstRow: number;
  readonly endRow: number;
  readonly firstCol: number;
  readonly endCol: number;
}

export const EMPTY_SPAN: Span = { firstRow: 0, endRow: 0, firstCol: 0, endCol: 0 };

/** A place along an axis: an item, and how much of it comes before. */
export interface AxisPlace {
  readonly index: number;
  /** From 0 to 1. */
  readonly fraction: number;
}

/** More than arithmetic in device pixels is off by, and less than can be seen. */
const ROUNDING = 1e-3;

/**
 * Sparse 1-D axis metrics (`14-m1b.md` 6.2):
 * - `O(1)` arithmetic when there are no custom row/column overrides;
 * - `O(log k)` binary search over compact prefix deltas when `k` rows/columns
 *   have custom pixel sizes.
 *
 * Given a device scale, sizes are whole device pixels and all sums are done in
 * those, so that an offset found from a position leads back to it whatever the
 * scale: `floor((p * d) / d)` is not always `p`. Sizes are given the way the
 * workbook has them, and come out enlarged by the zoom.
 */
export class SparseAxis {
  readonly defaultSize: number;
  readonly #indices: Uint32Array;
  // In device pixels with a device scale, and otherwise in CSS pixels.
  readonly #sizes: Float64Array;
  readonly #deltasPrefix: Float64Array;
  readonly #defaultUnits: number;
  readonly #unitsPerPixel: number;
  readonly #snaps: boolean;

  constructor(
    defaultSize: number,
    custom: readonly (readonly [number, number])[] = [],
    deviceScale?: number,
    zoom = 1,
  ) {
    this.#snaps = deviceScale !== undefined;
    this.#unitsPerPixel = deviceScale ?? 1;
    const units = (size: number) =>
      deviceScale === undefined ? size * zoom : deviceUnits(size * zoom, deviceScale);
    this.#defaultUnits = this.#snaps ? Math.max(1, units(defaultSize)) : units(defaultSize);
    this.defaultSize = this.#defaultUnits / this.#unitsPerPixel;
    const k = custom.length;
    this.#indices = new Uint32Array(k);
    this.#sizes = new Float64Array(k);
    this.#deltasPrefix = new Float64Array(k + 1);
    let acc = 0;
    for (let i = 0; i < k; i++) {
      const entry = custom[i];
      const idx = entry ? entry[0] : 0;
      const size = entry ? units(entry[1]) : this.#defaultUnits;
      this.#indices[i] = idx;
      this.#sizes[i] = size;
      acc += size - this.#defaultUnits;
      this.#deltasPrefix[i + 1] = acc;
    }
  }

  get customCount(): number {
    return this.#indices.length;
  }

  /** Pixel offset from start of axis to item `pos` (`0`-based). */
  offsetOf(pos: number): number {
    if (pos <= 0) {
      return 0;
    }
    const k = this.#indices.length;
    if (k === 0) {
      return (pos * this.#defaultUnits) / this.#unitsPerPixel;
    }
    const i = this.#lowerBound(pos);
    return (pos * this.#defaultUnits + (this.#deltasPrefix[i] ?? 0)) / this.#unitsPerPixel;
  }

  /** Pixel size of item `pos`. */
  sizeOf(pos: number): number {
    const k = this.#indices.length;
    if (k === 0) {
      return this.defaultSize;
    }
    const i = this.#lowerBound(pos);
    if (i < k && this.#indices[i] === pos) {
      return (this.#sizes[i] ?? this.#defaultUnits) / this.#unitsPerPixel;
    }
    return this.defaultSize;
  }

  /** Total pixel extent of `count` items (`0..count`). */
  totalSize(count: number): number {
    return this.offsetOf(Math.max(0, count));
  }

  /** `0`-based item index covering pixel offset `px >= 0`. */
  indexAtOffset(px: number): number {
    // The device pixel the offset is in. One that is meant to be the start of
    // a pixel may come out a little before it.
    const units = this.#snaps ? Math.floor(px * this.#unitsPerPixel + ROUNDING) : px;
    return this.#indexAtUnits(units);
  }

  /** Index after the last item that starts before pixel offset `px`. */
  endAtOffset(px: number): number {
    const before = this.#snaps ? Math.ceil(px * this.#unitsPerPixel - ROUNDING) - 1 : px - 0.0001;
    return this.#indexAtUnits(Math.max(0, before)) + 1;
  }

  /** Where in the sheet a place in an item is. */
  offsetOfPlace({ index, fraction }: AxisPlace): number {
    return this.offsetOf(index) + fraction * this.sizeOf(index);
  }

  /** Like `offsetOfPlace`, but the start of a pixel that is the item's. */
  pixelOfPlace({ index, fraction }: AxisPlace): number {
    const size = this.sizeOf(index);
    if (!this.#snaps) {
      return this.offsetOf(index) + fraction * size;
    }
    const units = Math.round(size * this.#unitsPerPixel);
    const inside = Math.max(0, Math.min(units - 1, Math.round(fraction * units)));
    return this.offsetOf(index) + inside / this.#unitsPerPixel;
  }

  /** The place of an offset among the first `count` items. */
  placeAt(px: number, count: number): AxisPlace {
    const at = Math.min(Math.max(0, px), this.totalSize(count));
    const index = Math.max(0, Math.min(count - 1, this.indexAtOffset(at)));
    const size = this.sizeOf(index);
    const fraction = size > 0 ? (at - this.offsetOf(index)) / size : 0;
    return { index, fraction: Math.min(1, Math.max(0, fraction)) };
  }

  #indexAtUnits(units: number): number {
    if (units <= 0) {
      return 0;
    }
    const k = this.#indices.length;
    if (k === 0) {
      return Math.floor(units / this.#defaultUnits);
    }
    // Binary search last custom entry `mid` whose start offset `<= units`.
    let lo = 0;
    let hi = k;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      const idx = this.#indices[mid] ?? 0;
      const start = idx * this.#defaultUnits + (this.#deltasPrefix[mid] ?? 0);
      if (start <= units) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    if (lo > 0) {
      const prev = lo - 1;
      const idx = this.#indices[prev] ?? 0;
      const start = idx * this.#defaultUnits + (this.#deltasPrefix[prev] ?? 0);
      const size = this.#sizes[prev] ?? this.#defaultUnits;
      if (units < start + size) {
        return idx;
      }
    }
    const delta = this.#deltasPrefix[lo] ?? 0;
    return Math.floor((units - delta) / this.#defaultUnits);
  }

  /** First `i` in `0..k` with `indices[i] >= pos`. */
  #lowerBound(pos: number): number {
    let lo = 0;
    let hi = this.#indices.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if ((this.#indices[mid] ?? 0) < pos) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    return lo;
  }
}

/** Optional per-sheet sparse metrics & freeze-pane configuration. */
export interface SheetLayout {
  readonly rowAxis: SparseAxis;
  readonly colAxis: SparseAxis;
  readonly frozenRows: number;
  readonly frozenCols: number;
}

export function defaultLayout(metrics: Metrics): SheetLayout {
  return {
    rowAxis: new SparseAxis(metrics.rowHeight),
    colAxis: new SparseAxis(metrics.colWidth),
    frozenRows: 0,
    frozenCols: 0,
  };
}

/** Clamped frozen pixel height and width that leave room for scrolling cells. */
export function frozenPixels(
  metrics: Metrics,
  sheet: SheetSize,
  size: Size,
  layout?: SheetLayout,
): { frozenRows: number; frozenCols: number; height: number; width: number } {
  if (!layout || (layout.frozenRows <= 0 && layout.frozenCols <= 0)) {
    return { frozenRows: 0, frozenCols: 0, height: 0, width: 0 };
  }
  const area = cellArea(metrics, size);
  const fRows = Math.min(sheet.rows, Math.max(0, layout.frozenRows));
  const fCols = Math.min(sheet.cols, Math.max(0, layout.frozenCols));
  const rawH = layout.rowAxis.offsetOf(fRows);
  const rawW = layout.colAxis.offsetOf(fCols);
  // If the viewport is smaller than the frozen pane itself, suppress freeze so
  // the sheet remains scrollable on compact windows.
  const useRows = rawH < area.height - 24 ? fRows : 0;
  const useCols = rawW < area.width - 32 ? fCols : 0;
  return {
    frozenRows: useRows,
    frozenCols: useCols,
    height: useRows > 0 ? rawH : 0,
    width: useCols > 0 ? rawW : 0,
  };
}

export function isEmpty(span: Span): boolean {
  return span.firstRow >= span.endRow || span.firstCol >= span.endCol;
}

export function sameSpan(a: Span, b: Span): boolean {
  return (
    a.firstRow === b.firstRow &&
    a.endRow === b.endRow &&
    a.firstCol === b.firstCol &&
    a.endCol === b.endCol
  );
}

/** The part of the grid's area that shows cells: all of it but the headers and scrollbars. */
export function cellArea(metrics: Metrics, size: Size): Size {
  return {
    width: Math.max(0, size.width - metrics.headerWidth - metrics.scrollbarSize),
    height: Math.max(0, size.height - metrics.headerHeight - metrics.scrollbarSize),
  };
}

/** Width and height of all cells together. */
export function contentSize(metrics: Metrics, sheet: SheetSize, layout?: SheetLayout): Size {
  if (!layout) {
    return { width: sheet.cols * metrics.colWidth, height: sheet.rows * metrics.rowHeight };
  }
  return {
    width: layout.colAxis.totalSize(sheet.cols),
    height: layout.rowAxis.totalSize(sheet.rows),
  };
}

/** The furthest one can scroll: the last row and column end up fully visible. */
export function scrollLimits(
  metrics: Metrics,
  sheet: SheetSize,
  size: Size,
  layout?: SheetLayout,
): Scroll {
  const content = contentSize(metrics, sheet, layout);
  const area = cellArea(metrics, size);
  return {
    top: Math.max(0, content.height - area.height),
    left: Math.max(0, content.width - area.width),
  };
}

export function clampScroll(scroll: Scroll, limits: Scroll): Scroll {
  return {
    top: Math.min(Math.max(0, scroll.top), limits.top),
    left: Math.min(Math.max(0, scroll.left), limits.left),
  };
}

/** The scrollable cells that are at least partly inside the unfrozen cell area. */
export function visibleCells(
  metrics: Metrics,
  sheet: SheetSize,
  scroll: Scroll,
  size: Size,
  layout?: SheetLayout,
): Span {
  const area = cellArea(metrics, size);
  if (area.width <= 0 || area.height <= 0 || sheet.cols <= 0) {
    return EMPTY_SPAN;
  }
  if (!layout) {
    return {
      firstRow: Math.min(sheet.rows, Math.floor(scroll.top / metrics.rowHeight)),
      endRow: Math.min(sheet.rows, Math.ceil((scroll.top + area.height) / metrics.rowHeight)),
      firstCol: Math.min(sheet.cols, Math.floor(scroll.left / metrics.colWidth)),
      endCol: Math.min(sheet.cols, Math.ceil((scroll.left + area.width) / metrics.colWidth)),
    };
  }
  const fz = frozenPixels(metrics, sheet, size, layout);
  const startTop = fz.height + scroll.top;
  const endTop = scroll.top + area.height;
  const startLeft = fz.width + scroll.left;
  const endLeft = scroll.left + area.width;

  const firstRow = Math.min(
    sheet.rows,
    Math.max(fz.frozenRows, layout.rowAxis.indexAtOffset(startTop)),
  );
  let endRow =
    endTop <= startTop ? firstRow : Math.min(sheet.rows, layout.rowAxis.endAtOffset(endTop));
  if (endRow < firstRow) {
    endRow = firstRow;
  }

  const firstCol = Math.min(
    sheet.cols,
    Math.max(fz.frozenCols, layout.colAxis.indexAtOffset(startLeft)),
  );
  let endCol =
    endLeft <= startLeft ? firstCol : Math.min(sheet.cols, layout.colAxis.endAtOffset(endLeft));
  if (endCol < firstCol) {
    endCol = firstCol;
  }

  return { firstRow, endRow, firstCol, endCol };
}

/**
 * The cell under a point of the grid's area (accounting for frozen panes and
 * sparse custom row/column sizes), or `null` over a header, scrollbar, or past
 * the last row/column.
 */
export function cellAt(
  metrics: Metrics,
  sheet: SheetSize,
  scroll: Scroll,
  size: Size,
  x: number,
  y: number,
  layout?: SheetLayout,
): { row: number; col: number } | null {
  const area = cellArea(metrics, size);
  const areaX = x - metrics.headerWidth;
  const areaY = y - metrics.headerHeight;
  if (areaX < 0 || areaY < 0 || areaX >= area.width || areaY >= area.height) {
    return null;
  }
  if (!layout) {
    const row = Math.floor((scroll.top + areaY) / metrics.rowHeight);
    const col = Math.floor((scroll.left + areaX) / metrics.colWidth);
    return row < sheet.rows && col < sheet.cols ? { row, col } : null;
  }
  const fz = frozenPixels(metrics, sheet, size, layout);
  const row =
    areaY < fz.height
      ? layout.rowAxis.indexAtOffset(areaY)
      : layout.rowAxis.indexAtOffset(scroll.top + areaY);
  const col =
    areaX < fz.width
      ? layout.colAxis.indexAtOffset(areaX)
      : layout.colAxis.indexAtOffset(scroll.left + areaX);
  return row < sheet.rows && col < sheet.cols ? { row, col } : null;
}

/** Hit-test result on the top/left header strips and corner button. */
export type HeaderHit =
  | { readonly kind: "corner" }
  | { readonly kind: "col"; readonly col: number }
  | { readonly kind: "colResize"; readonly col: number }
  | { readonly kind: "row"; readonly row: number }
  | { readonly kind: "rowResize"; readonly row: number };

const RESIZE_HANDLE_RADIUS = 4;
/** Headers smaller than this have no room for anything but themselves. */
const LEAST_GRIPPED = 10;
const FILL_HANDLE_RADIUS = 8;

/**
 * Whether a distance from the edge of something is within reach of its edge.
 * The middle half of everything is its own, however small it is.
 */
function isNearEdge(distance: number, radius: number, size: number): boolean {
  return distance <= radius && distance * 4 < size;
}

function isOnGrip(distance: number, size: number): boolean {
  return size >= LEAST_GRIPPED && isNearEdge(distance, RESIZE_HANDLE_RADIUS, size);
}

/** Classifies a pointer coordinate `(x, y)` inside the header strips (`null` if in body/scrollbars). */
export function headerHitAt(
  metrics: Metrics,
  sheet: SheetSize,
  scroll: Scroll,
  size: Size,
  x: number,
  y: number,
  layout?: SheetLayout,
): HeaderHit | null {
  const right = size.width - metrics.scrollbarSize;
  const bottom = size.height - metrics.scrollbarSize;
  if (x < 0 || y < 0 || x >= right || y >= bottom) {
    return null;
  }
  if (x < metrics.headerWidth && y < metrics.headerHeight) {
    return { kind: "corner" };
  }
  const resolved = layout ?? defaultLayout(metrics);
  const fz = frozenPixels(metrics, sheet, size, resolved);

  // Top column header strip.
  if (y < metrics.headerHeight && x >= metrics.headerWidth) {
    const areaX = x - metrics.headerWidth;
    const sheetX = areaX < fz.width ? areaX : scroll.left + areaX;
    const col = resolved.colAxis.indexAtOffset(sheetX);
    if (col >= sheet.cols) {
      // Allow grabbing the right edge of the very last column!
      if (sheet.cols > 0) {
        const lastRight = resolved.colAxis.offsetOf(sheet.cols);
        if (Math.abs(sheetX - lastRight) <= RESIZE_HANDLE_RADIUS) {
          return { kind: "colResize", col: sheet.cols - 1 };
        }
      }
      return null;
    }
    const colStart = resolved.colAxis.offsetOf(col);
    const colSize = resolved.colAxis.sizeOf(col);
    if (isOnGrip(colStart + colSize - sheetX, colSize)) {
      return { kind: "colResize", col };
    }
    if (col > 0 && isOnGrip(sheetX - colStart, colSize)) {
      // Do not cross the freeze boundary to resize a scrolled-under column when over frozen strip.
      if (!(areaX >= fz.width && col - 1 < fz.frozenCols && scroll.left > 0)) {
        return { kind: "colResize", col: col - 1 };
      }
    }
    return { kind: "col", col };
  }

  // Left row header strip.
  if (x < metrics.headerWidth && y >= metrics.headerHeight) {
    const areaY = y - metrics.headerHeight;
    const sheetY = areaY < fz.height ? areaY : scroll.top + areaY;
    const row = resolved.rowAxis.indexAtOffset(sheetY);
    if (row >= sheet.rows) {
      if (sheet.rows > 0) {
        const lastBottom = resolved.rowAxis.offsetOf(sheet.rows);
        if (Math.abs(sheetY - lastBottom) <= RESIZE_HANDLE_RADIUS) {
          return { kind: "rowResize", row: sheet.rows - 1 };
        }
      }
      return null;
    }
    const rowStart = resolved.rowAxis.offsetOf(row);
    const rowSize = resolved.rowAxis.sizeOf(row);
    if (isOnGrip(rowStart + rowSize - sheetY, rowSize)) {
      return { kind: "rowResize", row };
    }
    if (row > 0 && isOnGrip(sheetY - rowStart, rowSize)) {
      if (!(areaY >= fz.height && row - 1 < fz.frozenRows && scroll.top > 0)) {
        return { kind: "rowResize", row: row - 1 };
      }
    }
    return { kind: "row", row };
  }

  return null;
}

/** A rectangle in CSS pixels, measured from the top left corner of the grid's area. */
export interface Box {
  readonly left: number;
  readonly top: number;
  readonly width: number;
  readonly height: number;
}

/** Where a cell is drawn (accounting for frozen panes and sparse custom sizes). */
export function cellBox(
  metrics: Metrics,
  scroll: Scroll,
  row: number,
  col: number,
  size?: Size,
  sheet?: SheetSize,
  layout?: SheetLayout,
): Box {
  if (!layout) {
    return {
      left: metrics.headerWidth + col * metrics.colWidth - scroll.left,
      top: metrics.headerHeight + row * metrics.rowHeight - scroll.top,
      width: metrics.colWidth,
      height: metrics.rowHeight,
    };
  }
  const fz =
    size && sheet
      ? frozenPixels(metrics, sheet, size, layout)
      : { frozenRows: layout.frozenRows, frozenCols: layout.frozenCols, height: 0, width: 0 };
  const effScrollTop = row < fz.frozenRows ? 0 : scroll.top;
  const effScrollLeft = col < fz.frozenCols ? 0 : scroll.left;
  return {
    left: metrics.headerWidth + layout.colAxis.offsetOf(col) - effScrollLeft,
    top: metrics.headerHeight + layout.rowAxis.offsetOf(row) - effScrollTop,
    width: layout.colAxis.sizeOf(col),
    height: layout.rowAxis.sizeOf(row),
  };
}

/** Everything the place of a cell on the screen depends on. */
export interface View {
  readonly metrics: Metrics;
  readonly sheet: SheetSize;
  readonly size: Size;
  readonly layout: SheetLayout;
  readonly scroll: Scroll;
}

/** A position in CSS pixels from the top left corner of the grid's area. */
export interface Point {
  readonly x: number;
  readonly y: number;
}

/**
 * Whether a point is over the handle at the last corner of some selected cells.
 * Half of it is on the cells that come next on the screen: past the last frozen
 * ones, those are the first in view of the ones that scroll.
 */
export function isOverFillHandle(view: View, range: Span, { x, y }: Point): boolean {
  const { metrics, sheet, size, layout, scroll } = view;
  if (isEmpty(range) || x < metrics.headerWidth || y < metrics.headerHeight) {
    return false;
  }
  const row = range.endRow - 1;
  const col = range.endCol - 1;
  const frozen = frozenPixels(metrics, sheet, size, layout);
  const follows = (index: number, last: number, frozenCount: number) =>
    index === last + 1 || (last === frozenCount - 1 && index > last);
  const corner = cellBox(metrics, scroll, row, col, size, sheet, layout);
  const dx = Math.abs(x - corner.left - corner.width);
  const dy = Math.abs(y - corner.top - corner.height);
  const cell = cellAt(metrics, sheet, scroll, size, x, y, layout);
  if (!cell) {
    // Past the last row or column, where there is nothing else to point at.
    return dx <= FILL_HANDLE_RADIUS && dy <= FILL_HANDLE_RADIUS;
  }
  return (
    (cell.row === row || follows(cell.row, row, frozen.frozenRows)) &&
    (cell.col === col || follows(cell.col, col, frozen.frozenCols)) &&
    isNearEdge(dx, FILL_HANDLE_RADIUS, layout.colAxis.sizeOf(cell.col)) &&
    isNearEdge(dy, FILL_HANDLE_RADIUS, layout.rowAxis.sizeOf(cell.row))
  );
}

/** One axis of a view. */
interface AxisView {
  readonly axis: SparseAxis;
  readonly count: number;
  /** Where the cells begin, and where the frozen ones among them end. */
  readonly header: number;
  readonly frozen: number;
  readonly room: number;
  readonly scroll: number;
}

function axesOf(view: View): { readonly rows: AxisView; readonly cols: AxisView } {
  const { metrics, sheet, size, layout, scroll } = view;
  const area = cellArea(metrics, size);
  const frozen = frozenPixels(metrics, sheet, size, layout);
  return {
    rows: {
      axis: layout.rowAxis,
      count: sheet.rows,
      header: metrics.headerHeight,
      frozen: frozen.height,
      room: area.height,
      scroll: scroll.top,
    },
    cols: {
      axis: layout.colAxis,
      count: sheet.cols,
      header: metrics.headerWidth,
      frozen: frozen.width,
      room: area.width,
      scroll: scroll.left,
    },
  };
}

/** What is in view of some rows or columns: the first and last of them may be frozen and not. */
function shownOf(
  { axis, header, frozen, room, scroll }: AxisView,
  frozenCount: number,
  first: number,
  end: number,
): { readonly start: number; readonly end: number } | null {
  const pinned = {
    start: axis.offsetOf(Math.min(first, frozenCount)),
    end: axis.offsetOf(Math.min(end, frozenCount)),
  };
  const scrolled = {
    start: Math.max(frozen, axis.offsetOf(Math.max(first, frozenCount)) - scroll),
    end: Math.min(room, axis.offsetOf(Math.max(end, frozenCount)) - scroll),
  };
  const shown = [pinned, scrolled].filter((part) => part.start < part.end);
  const [from] = shown;
  const to = shown.at(-1);
  return from && to ? { start: header + from.start, end: header + to.end } : null;
}

/** What is in view of some cells, or `null` if nothing is. */
export function boxOfSpan(view: View, span: Span): Box | null {
  const frozen = frozenPixels(view.metrics, view.sheet, view.size, view.layout);
  const { rows, cols } = axesOf(view);
  const y = shownOf(rows, frozen.frozenRows, span.firstRow, span.endRow);
  const x = shownOf(cols, frozen.frozenCols, span.firstCol, span.endCol);
  return x && y
    ? { left: x.start, top: y.start, width: x.end - x.start, height: y.end - y.start }
    : null;
}

/** The sheet's place in the corner of the part of a view that scrolls. */
export interface Corner {
  readonly row: AxisPlace;
  readonly col: AxisPlace;
}

export function cornerOf(view: View): Corner {
  const { rows, cols } = axesOf(view);
  const first = ({ axis, count, frozen, scroll }: AxisView) => axis.placeAt(scroll + frozen, count);
  return { row: first(rows), col: first(cols) };
}

/** The scroll position that has a place in the corner, as far as the sheet allows. */
export function scrollToCorner(view: View, corner: Corner): Scroll {
  const { rows, cols } = axesOf(view);
  const scroll = {
    top: rows.axis.pixelOfPlace(corner.row) - rows.frozen,
    left: cols.axis.pixelOfPlace(corner.col) - cols.frozen,
  };
  return clampScroll(scroll, scrollLimits(view.metrics, view.sheet, view.size, view.layout));
}

function anchored(before: AxisView, after: AxisView, point: number | null): number {
  const at = point === null ? -1 : Math.min(point - before.header, before.room);
  if (point === null || at < before.frozen) {
    // What is frozen stays where it is anyway.
    const corner = before.axis.placeAt(before.scroll + before.frozen, before.count);
    return after.axis.pixelOfPlace(corner) - after.frozen;
  }
  const place = before.axis.placeAt(before.scroll + at, before.count);
  const wanted = before.header + at - after.header;
  return after.axis.offsetOfPlace(place) - Math.max(after.frozen, wanted);
}

/**
 * The scroll position at which a view that has changed its sizes shows, at a
 * point, what it showed there before. Over the headers and the frozen cells,
 * or without a point, that is its corner's place.
 */
export function reanchor(before: View, after: View, point: Point | null): Scroll {
  const from = axesOf(before);
  const to = axesOf(after);
  const scroll = {
    top: anchored(from.rows, to.rows, point?.y ?? null),
    left: anchored(from.cols, to.cols, point?.x ?? null),
  };
  return clampScroll(scroll, scrollLimits(after.metrics, after.sheet, after.size, after.layout));
}

/** The middle of what is in view of a cell, if anything is. */
export function pointOfCell(
  view: View,
  cell: { readonly row: number; readonly col: number },
): Point | null {
  const { metrics, sheet, size, layout, scroll } = view;
  const frozen = frozenPixels(metrics, sheet, size, layout);
  const { rows, cols } = axesOf(view);
  const box = cellBox(metrics, scroll, cell.row, cell.col, size, sheet, layout);
  const least = (along: AxisView, isFrozen: boolean) =>
    along.header + (isFrozen ? 0 : along.frozen);
  const left = Math.max(box.left, least(cols, cell.col < frozen.frozenCols));
  const top = Math.max(box.top, least(rows, cell.row < frozen.frozenRows));
  const right = Math.min(box.left + box.width, cols.header + cols.room);
  const bottom = Math.min(box.top + box.height, rows.header + rows.room);
  return left < right && top < bottom ? { x: (left + right) / 2, y: (top + bottom) / 2 } : null;
}

/** The middle of the part of a view that scrolls. */
export function middleOf(view: View): Point {
  const { rows, cols } = axesOf(view);
  const along = ({ header, frozen, room }: AxisView) => header + (frozen + room) / 2;
  return { x: along(cols), y: along(rows) };
}

/** Where a cell's editor is, and how large what is in it. */
export interface EditorBox extends Box {
  /** What is enlarged by: at least what a font of a legible size takes. */
  readonly zoom: number;
  /** How far it may grow with its text. */
  readonly maxWidth: number;
}

/**
 * The editor of a cell. It is as large as the cell if text of a legible size
 * fits into that, and floats over the cells around it otherwise, inside the
 * cells' area. Shown, it stays on its cell even if that is scrolled away;
 * hidden, it is somewhere in view, where an input method can open its window.
 */
export function editorBox(
  metrics: Metrics,
  size: Size,
  cell: Box,
  zoom: number,
  leastZoom: number,
  deviceScale: number,
  shown: boolean,
): EditorBox {
  const area = cellArea(metrics, size);
  const enlarged = Math.max(zoom, leastZoom);
  const along = (from: number, extent: number, lead: number, header: number, room: number) => {
    const length = deviceUnits(extent * (enlarged / zoom), deviceScale) / deviceScale;
    const spare = Math.floor((length - extent) * lead * deviceScale) / deviceScale;
    const start = Math.max(header, Math.min(from - spare, header + room - length));
    return {
      start: shown ? Math.min(from, Math.max(start, from + extent - length)) : start,
      length,
    };
  };
  // What is typed starts where the cell's text does, and is on a line with it.
  const x = along(cell.left, cell.width, 0, metrics.headerWidth, area.width);
  const y = along(cell.top, cell.height, 0.5, metrics.headerHeight, area.height);
  return {
    left: x.start,
    top: y.start,
    width: x.length,
    height: y.length,
    zoom: enlarged,
    maxWidth: Math.max(x.length, metrics.headerWidth + area.width - x.start),
  };
}

/** Every how many of a header's cells there is a label, so that the labels have room: 1, 2, 5, 10... */
export function labelStep(size: number, needed: number): number {
  const least = needed / Math.max(1, size);
  const magnitude = 10 ** Math.max(0, Math.floor(Math.log10(Math.max(1, least))));
  return magnitude * ([1, 2, 5].find((step) => step * magnitude >= least) ?? 10);
}

/**
 * The scroll position nearest to `scroll` at which a cell is fully in view
 * (frozen rows/columns are already pinned in view and require zero scroll on
 * their pinned axis).
 */
export function scrollToReveal(
  metrics: Metrics,
  size: Size,
  scroll: Scroll,
  row: number,
  col: number,
  sheet?: SheetSize,
  layout?: SheetLayout,
): Scroll {
  const area = cellArea(metrics, size);
  if (!layout) {
    const alongUniform = (position: number, extent: number, at: number, room: number) => {
      const start = at * extent;
      if (start < position) {
        return start;
      }
      return Math.max(position, Math.min(start, start + extent - room));
    };
    return {
      top: alongUniform(scroll.top, metrics.rowHeight, row, area.height),
      left: alongUniform(scroll.left, metrics.colWidth, col, area.width),
    };
  }

  const fz = sheet
    ? frozenPixels(metrics, sheet, size, layout)
    : {
        frozenRows: layout.frozenRows,
        frozenCols: layout.frozenCols,
        height: layout.rowAxis.offsetOf(layout.frozenRows),
        width: layout.colAxis.offsetOf(layout.frozenCols),
      };

  const revealAlong = (
    curScroll: number,
    axis: SparseAxis,
    index: number,
    frozenCount: number,
    frozenExtent: number,
    totalRoom: number,
  ): number => {
    if (index < frozenCount) {
      return curScroll;
    }
    const start = axis.offsetOf(index) - frozenExtent;
    const len = axis.sizeOf(index);
    const room = Math.max(0, totalRoom - frozenExtent);
    if (start < curScroll) {
      return Math.max(0, start);
    }
    return Math.max(curScroll, Math.min(start, start + len - room));
  };

  return {
    top: revealAlong(scroll.top, layout.rowAxis, row, fz.frozenRows, fz.height, area.height),
    left: revealAlong(scroll.left, layout.colAxis, col, fz.frozenCols, fz.width, area.width),
  };
}

/** Pages along one axis: the visible ones, and how many to hold on either side. */
interface HeldPages {
  readonly first: number;
  readonly end: number;
  before: number;
  after: number;
}

function heldAlong(
  first: number,
  end: number,
  extent: number,
  margin: number,
  page: number,
  limit: number,
): HeldPages {
  const cells = Math.ceil((end - first) * margin);
  const firstPage = Math.floor(first / page);
  const endPage = Math.min(Math.ceil(end / page), firstPage + MAX_VISIBLE_PAGES_PER_AXIS);
  const held = {
    first: firstPage,
    end: endPage,
    before: firstPage - Math.floor(Math.max(0, first - cells) / page),
    after: Math.max(0, Math.ceil(Math.min(extent, end + cells) / page) - endPage),
  };
  // A side that has less to hold than its half leaves the rest to the other one.
  const room = Math.max(0, limit - (endPage - firstPage));
  if (held.before + held.after > room) {
    const half = Math.floor(room / 2);
    if (held.before <= half) {
      held.after = room - held.before;
    } else if (held.after <= room - half) {
      held.before = room - held.after;
    } else {
      held.before = half;
      held.after = room - half;
    }
  }
  return held;
}

const heldLength = (pages: HeldPages): number =>
  pages.end - pages.first + pages.before + pages.after;

/** Gives up a page on a side with `distance` pages of margin, if there is one. */
function cutAt(pages: HeldPages, distance: number): boolean {
  if (pages.before === distance) {
    pages.before--;
    return true;
  }
  if (pages.after === distance) {
    pages.after--;
    return true;
  }
  return false;
}

/**
 * The pages to hold for a range of visible cells: the pages those cells are
 * in, and around them a margin measured in multiples of the visible range. The
 * visible pages are held however many they are; the margin is cut, where it is
 * furthest from them, to what they leave of the limits. The engine applies the
 * same rule, so it sends what is held and nothing else.
 */
export function pagesToHold(
  visible: Span,
  sheet: SheetSize,
  margin: { rows: number; cols: number },
): Span {
  if (isEmpty(visible)) {
    return EMPTY_SPAN;
  }
  const rows = heldAlong(
    visible.firstRow,
    visible.endRow,
    sheet.rows,
    margin.rows,
    PAGE_ROWS,
    MAX_SUBSCRIBED_PAGE_ROWS,
  );
  const cols = heldAlong(
    visible.firstCol,
    visible.endCol,
    sheet.cols,
    margin.cols,
    PAGE_COLS,
    MAX_SUBSCRIBED_PAGE_COLS,
  );
  const limit = Math.max(MAX_SUBSCRIBED_PAGES, (rows.end - rows.first) * (cols.end - cols.first));
  while (heldLength(rows) * heldLength(cols) > limit) {
    const furthest = Math.max(rows.before, rows.after, cols.before, cols.after);
    if (!cutAt(cols, furthest)) {
      cutAt(rows, furthest);
    }
  }
  return {
    firstRow: rows.first - rows.before,
    endRow: rows.end + rows.after,
    firstCol: cols.first - cols.before,
    endCol: cols.end + cols.after,
  };
}

/** The cells a span of pages holds, as far as the sheet goes. */
export function cellsOfPages(pages: Span, sheet: SheetSize): Span {
  if (isEmpty(pages)) {
    return EMPTY_SPAN;
  }
  return {
    firstRow: pages.firstRow * PAGE_ROWS,
    endRow: Math.min(sheet.rows, pages.endRow * PAGE_ROWS),
    firstCol: pages.firstCol * PAGE_COLS,
    endCol: Math.min(sheet.cols, pages.endCol * PAGE_COLS),
  };
}

/** Where a scrollbar's thumb sits in its track, or `null` when there is nothing to scroll. */
export function thumb(
  track: number,
  content: number,
  viewport: number,
  scroll: number,
  minLength: number,
): { offset: number; length: number } | null {
  const range = content - viewport;
  if (range <= 0 || track <= 0) {
    return null;
  }
  const length = Math.min(track, Math.max(minLength, (track * viewport) / content));
  const travel = track - length;
  return { offset: travel * Math.min(1, Math.max(0, scroll / range)), length };
}

/** The scroll position that puts the thumb at `offset`: the inverse of {@link thumb}. */
export function scrollAtThumb(
  track: number,
  content: number,
  viewport: number,
  offset: number,
  length: number,
): number {
  const range = Math.max(0, content - viewport);
  const travel = track - length;
  return travel <= 0 ? 0 : range * Math.min(1, Math.max(0, offset / travel));
}

/** The name of a column: A to Z, then AA, AB and so on. */
export function columnName(col: number): string {
  let name = "";
  for (let rest = col; rest >= 0; rest = Math.floor(rest / 26) - 1) {
    name = String.fromCharCode(65 + (rest % 26)) + name;
  }
  return name;
}
