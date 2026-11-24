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

/** Default sizes in CSS pixels. */
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

/**
 * Sparse 1-D axis metrics (`14-m1b.md` 6.2):
 * - `O(1)` arithmetic when there are no custom row/column overrides;
 * - `O(log k)` binary search over compact prefix deltas when `k` rows/columns
 *   have custom pixel sizes.
 *
 * Given a device scale, sizes are whole device pixels and all sums are done in
 * those, so that an offset found from a position leads back to it whatever the
 * scale: `floor((p * d) / d)` is not always `p`.
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
  ) {
    this.#snaps = deviceScale !== undefined;
    this.#unitsPerPixel = deviceScale ?? 1;
    this.#defaultUnits = this.#snaps ? Math.max(1, this.#toUnits(defaultSize)) : defaultSize;
    this.defaultSize = this.#defaultUnits / this.#unitsPerPixel;
    const k = custom.length;
    this.#indices = new Uint32Array(k);
    this.#sizes = new Float64Array(k);
    this.#deltasPrefix = new Float64Array(k + 1);
    let acc = 0;
    for (let i = 0; i < k; i++) {
      const entry = custom[i];
      const idx = entry ? entry[0] : 0;
      const units = entry ? this.#toUnits(entry[1]) : this.#defaultUnits;
      this.#indices[i] = idx;
      this.#sizes[i] = units;
      acc += units - this.#defaultUnits;
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
    return this.#indexAtUnits(this.#toUnits(px));
  }

  /** Index after the last item that starts before pixel offset `px`. */
  endAtOffset(px: number): number {
    const before = this.#snaps ? this.#toUnits(px) - 1 : px - 0.0001;
    return this.#indexAtUnits(Math.max(0, before)) + 1;
  }

  #toUnits(px: number): number {
    return this.#snaps ? Math.round(px * this.#unitsPerPixel) : px;
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
  if (area.width <= 0 || area.height <= 0 || sheet.rows <= 0 || sheet.cols <= 0) {
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
    const colEnd = colStart + resolved.colAxis.sizeOf(col);
    if (Math.abs(sheetX - colEnd) <= RESIZE_HANDLE_RADIUS) {
      return { kind: "colResize", col };
    }
    if (col > 0 && Math.abs(sheetX - colStart) <= RESIZE_HANDLE_RADIUS) {
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
    const rowEnd = rowStart + resolved.rowAxis.sizeOf(row);
    if (Math.abs(sheetY - rowEnd) <= RESIZE_HANDLE_RADIUS) {
      return { kind: "rowResize", row };
    }
    if (row > 0 && Math.abs(sheetY - rowStart) <= RESIZE_HANDLE_RADIUS) {
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
