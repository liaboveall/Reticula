// The arithmetic of the grid: which cells are on screen, what a pixel points
// at, how far one can scroll. Everything here is a pure function of numbers,
// so it is tested without a browser.

import {
  MAX_SUBSCRIBED_PAGE_COLS,
  MAX_SUBSCRIBED_PAGE_ROWS,
  PAGE_COLS,
  PAGE_ROWS,
} from "@reticula/protocol/viewport";

/** Sizes in CSS pixels. Rows and columns are uniform for now. */
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
export function contentSize(metrics: Metrics, sheet: SheetSize): Size {
  return { width: sheet.cols * metrics.colWidth, height: sheet.rows * metrics.rowHeight };
}

/** The furthest one can scroll: the last row and column end up fully visible. */
export function scrollLimits(metrics: Metrics, sheet: SheetSize, size: Size): Scroll {
  const content = contentSize(metrics, sheet);
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

/** The cells that are at least partly inside the cell area. */
export function visibleCells(metrics: Metrics, sheet: SheetSize, scroll: Scroll, size: Size): Span {
  const area = cellArea(metrics, size);
  if (area.width <= 0 || area.height <= 0 || sheet.rows <= 0 || sheet.cols <= 0) {
    return EMPTY_SPAN;
  }
  return {
    firstRow: Math.min(sheet.rows, Math.floor(scroll.top / metrics.rowHeight)),
    endRow: Math.min(sheet.rows, Math.ceil((scroll.top + area.height) / metrics.rowHeight)),
    firstCol: Math.min(sheet.cols, Math.floor(scroll.left / metrics.colWidth)),
    endCol: Math.min(sheet.cols, Math.ceil((scroll.left + area.width) / metrics.colWidth)),
  };
}

/**
 * The cell under a point of the grid's area, or `null` over a header, a
 * scrollbar, or the space past the last row or column.
 */
export function cellAt(
  metrics: Metrics,
  sheet: SheetSize,
  scroll: Scroll,
  size: Size,
  x: number,
  y: number,
): { row: number; col: number } | null {
  const area = cellArea(metrics, size);
  const areaX = x - metrics.headerWidth;
  const areaY = y - metrics.headerHeight;
  if (areaX < 0 || areaY < 0 || areaX >= area.width || areaY >= area.height) {
    return null;
  }
  const row = Math.floor((scroll.top + areaY) / metrics.rowHeight);
  const col = Math.floor((scroll.left + areaX) / metrics.colWidth);
  return row < sheet.rows && col < sheet.cols ? { row, col } : null;
}

/** A rectangle in CSS pixels, measured from the top left corner of the grid's area. */
export interface Box {
  readonly left: number;
  readonly top: number;
  readonly width: number;
  readonly height: number;
}

/** Where a cell is. A cell that is scrolled out of view has a box outside the cell area. */
export function cellBox(metrics: Metrics, scroll: Scroll, row: number, col: number): Box {
  return {
    left: metrics.headerWidth + col * metrics.colWidth - scroll.left,
    top: metrics.headerHeight + row * metrics.rowHeight - scroll.top,
    width: metrics.colWidth,
    height: metrics.rowHeight,
  };
}

/**
 * The scroll position nearest to `scroll` at which a cell is fully in view:
 * unchanged if it already is, otherwise moved just far enough.
 */
export function scrollToReveal(
  metrics: Metrics,
  size: Size,
  scroll: Scroll,
  row: number,
  col: number,
): Scroll {
  const area = cellArea(metrics, size);
  const along = (position: number, extent: number, at: number, room: number) => {
    const start = at * extent;
    if (start < position) {
      return start;
    }
    // A cell larger than the area is aligned at its start.
    return Math.max(position, Math.min(start, start + extent - room));
  };
  return {
    top: along(scroll.top, metrics.rowHeight, row, area.height),
    left: along(scroll.left, metrics.colWidth, col, area.width),
  };
}

/**
 * The pages to hold for a range of visible cells: the pages those cells are
 * in, and around them a margin measured in multiples of the visible range.
 * The margin is what lets scrolling find its data already there.
 *
 * The engine computes the same span from the range it is sent (see
 * `PageRange::covering` in the engine); the two must agree, because the grid
 * waits for exactly these pages.
 */
export function pagesToHold(
  visible: Span,
  sheet: SheetSize,
  margin: { rows: number; cols: number },
): Span {
  if (isEmpty(visible)) {
    return EMPTY_SPAN;
  }
  const marginRows = Math.ceil((visible.endRow - visible.firstRow) * margin.rows);
  const marginCols = Math.ceil((visible.endCol - visible.firstCol) * margin.cols);
  const firstRow = Math.floor(Math.max(0, visible.firstRow - marginRows) / PAGE_ROWS);
  const firstCol = Math.floor(Math.max(0, visible.firstCol - marginCols) / PAGE_COLS);
  const lastRow = Math.min(sheet.rows, visible.endRow + marginRows) - 1;
  const lastCol = Math.min(sheet.cols, visible.endCol + marginCols) - 1;
  return {
    firstRow,
    endRow: Math.min(Math.floor(lastRow / PAGE_ROWS) + 1, firstRow + MAX_SUBSCRIBED_PAGE_ROWS),
    firstCol,
    endCol: Math.min(Math.floor(lastCol / PAGE_COLS) + 1, firstCol + MAX_SUBSCRIBED_PAGE_COLS),
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
  // Proportional to the visible share, but never too small to grab: with a
  // million rows the proportional thumb would be a fraction of a pixel.
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
