import { PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import type { PageCache } from "./cache.ts";
import { columnName, isEmpty, type Metrics, type Scroll, type Size, type Span } from "./layout.ts";
import { isNumber } from "./page.ts";
import type { Theme } from "./theme.ts";

/** Everything one frame is drawn from. */
export interface Frame {
  /** Size of the grid's area in CSS pixels. */
  readonly size: Size;
  /** Device pixels per CSS pixel. */
  readonly scale: number;
  readonly metrics: Metrics;
  readonly theme: Theme;
  /** Scroll position, already aligned to device pixels. */
  readonly scroll: Scroll;
  /** The cells at least partly in view. */
  readonly visible: Span;
  readonly cache: PageCache;
  readonly selection: { readonly row: number; readonly col: number } | null;
}

export interface FrameCounts {
  /** Cells in view whose text was drawn. */
  cells: number;
  /** Of those, the cells whose text was drawn through a clip. */
  clipped: number;
  /** Cells in view whose page has not arrived; they are shown as placeholders. */
  missing: number;
}

/**
 * The renderer's shortcuts. All of them are on, unless a measurement turns
 * one off to find out what it is worth.
 */
export interface Shortcuts {
  /** Remember the measured width of each string. */
  readonly cacheWidths: boolean;
  /** Skip measuring a number whose length alone shows that it fits. */
  readonly boundNumbers: boolean;
  /** Clip only text that is wider than its cell, not every text. */
  readonly clipOnlyOverwide: boolean;
  /** Set the drawing state when it changes, not for every cell. */
  readonly batchState: boolean;
}

export const ALL_SHORTCUTS: Shortcuts = {
  cacheWidths: true,
  boundNumbers: true,
  clipOnlyOverwide: true,
  batchState: true,
};

/** Space between a cell's edge and its text. */
const PADDING = 6;

/** Every character a formatted number can contain. */
const NUMBER_ALPHABET = "0123456789.,-+%eE";

/**
 * Widths of strings in one font.
 *
 * Whether a text fits its cell decides between a plain draw and a clipped
 * one, and clipping is the expensive path. Measuring is not free either, so
 * the common case is answered without it: a number is drawn from a small
 * alphabet, and its length times the widest of those characters is a bound
 * that most numbers already pass.
 */
class TextWidths {
  #shortcuts: Shortcuts;
  #font = "";
  #widths = new Map<string, number>();
  #numberAdvance = 0;

  constructor(shortcuts: Shortcuts) {
    this.#shortcuts = shortcuts;
  }

  /** Call with the context's current font before asking for widths. */
  use(context: CanvasRenderingContext2D, font: string): void {
    if (font === this.#font) {
      return;
    }
    this.#font = font;
    this.#widths.clear();
    let widest = 0;
    for (const character of NUMBER_ALPHABET) {
      widest = Math.max(widest, context.measureText(character).width);
    }
    this.#numberAdvance = widest;
  }

  fits(context: CanvasRenderingContext2D, text: string, number: boolean, room: number): boolean {
    if (this.#shortcuts.boundNumbers && number && text.length * this.#numberAdvance <= room) {
      return true;
    }
    if (!this.#shortcuts.cacheWidths) {
      return context.measureText(text).width <= room;
    }
    let width = this.#widths.get(text);
    if (width === undefined) {
      width = context.measureText(text).width;
      if (this.#widths.size >= 8192) {
        this.#widths.clear();
      }
      this.#widths.set(text, width);
    }
    return width <= room;
  }
}

/** The part of one page that is in view. */
interface Part {
  readonly pageRow: number;
  readonly pageCol: number;
  readonly firstRow: number;
  readonly endRow: number;
  readonly firstCol: number;
  readonly endCol: number;
}

/** The pages the visible cells are in, each cut down to what is visible of it. */
function partsOf(visible: Span): Part[] {
  const parts: Part[] = [];
  const lastPageRow = Math.floor((visible.endRow - 1) / PAGE_ROWS);
  const lastPageCol = Math.floor((visible.endCol - 1) / PAGE_COLS);
  for (let pageRow = Math.floor(visible.firstRow / PAGE_ROWS); pageRow <= lastPageRow; pageRow++) {
    for (
      let pageCol = Math.floor(visible.firstCol / PAGE_COLS);
      pageCol <= lastPageCol;
      pageCol++
    ) {
      parts.push({
        pageRow,
        pageCol,
        firstRow: Math.max(visible.firstRow, pageRow * PAGE_ROWS),
        endRow: Math.min(visible.endRow, (pageRow + 1) * PAGE_ROWS),
        firstCol: Math.max(visible.firstCol, pageCol * PAGE_COLS),
        endCol: Math.min(visible.endCol, (pageCol + 1) * PAGE_COLS),
      });
    }
  }
  return parts;
}

/** Draws frames onto one canvas. */
export class Renderer {
  #context: CanvasRenderingContext2D;
  #shortcuts: Shortcuts;
  #widths: TextWidths;

  constructor(canvas: HTMLCanvasElement, shortcuts: Shortcuts = ALL_SHORTCUTS) {
    // The grid is opaque; saying so lets the browser skip blending the canvas.
    const context = canvas.getContext("2d", { alpha: false });
    if (!context) {
      throw new Error("this browser cannot draw on a 2D canvas");
    }
    this.#context = context;
    this.#shortcuts = shortcuts;
    this.#widths = new TextWidths(shortcuts);
  }

  draw(frame: Frame): FrameCounts {
    const context = this.#context;
    const { size, scale, metrics, theme, scroll, visible } = frame;
    const counts: FrameCounts = { cells: 0, clipped: 0, missing: 0 };

    context.setTransform(scale, 0, 0, scale, 0, 0);
    context.fillStyle = theme.background;
    context.fillRect(0, 0, size.width, size.height);

    // Where the top left corner of cell (0, 0) is.
    const originX = metrics.headerWidth - scroll.left;
    const originY = metrics.headerHeight - scroll.top;
    const right = size.width - metrics.scrollbarSize;
    const bottom = size.height - metrics.scrollbarSize;

    if (!isEmpty(visible)) {
      // Fills, then lines, then text: text that is wider than its cell will
      // one day run across the lines of the empty cells beside it.
      const parts = partsOf(visible);
      this.#placeholders(frame, parts, originX, originY, counts);
      this.#lines(frame, originX, originY, right, bottom);
      this.#text(frame, parts, originX, originY, counts);
      this.#selection(frame, originX, originY);
    }
    this.#headers(frame, originX, originY, right, bottom);
    return counts;
  }

  /** A plain fill where a page has not arrived. */
  #placeholders(
    frame: Frame,
    parts: readonly Part[],
    originX: number,
    originY: number,
    counts: FrameCounts,
  ): void {
    const context = this.#context;
    const { rowHeight, colWidth } = frame.metrics;
    context.fillStyle = frame.theme.placeholder;
    for (const part of parts) {
      if (frame.cache.get(part.pageRow, part.pageCol)) {
        continue;
      }
      context.fillRect(
        originX + part.firstCol * colWidth,
        originY + part.firstRow * rowHeight,
        (part.endCol - part.firstCol) * colWidth,
        (part.endRow - part.firstRow) * rowHeight,
      );
      counts.missing += (part.endRow - part.firstRow) * (part.endCol - part.firstCol);
    }
  }

  /** The text of the cells, page by page. */
  #text(
    frame: Frame,
    parts: readonly Part[],
    originX: number,
    originY: number,
    counts: FrameCounts,
  ): void {
    const context = this.#context;
    const { metrics, theme, cache } = frame;
    const { rowHeight, colWidth } = metrics;
    const { clipOnlyOverwide, batchState } = this.#shortcuts;
    const room = colWidth - 2 * PADDING;

    context.font = theme.font;
    context.textBaseline = "middle";
    context.fillStyle = theme.text;
    this.#widths.use(context, theme.font);
    let aligned: CanvasTextAlign | null = null;

    for (const part of parts) {
      const page = cache.get(part.pageRow, part.pageCol);
      if (!page) {
        continue;
      }
      // Column by column: a column mostly holds one kind of value, so the
      // alignment rarely changes from one cell to the next.
      for (let col = part.firstCol; col < part.endCol; col++) {
        const x = originX + col * colWidth;
        for (let row = part.firstRow; row < part.endRow; row++) {
          const cell = (row - page.firstRow) * page.colCount + (col - page.firstCol);
          const text = page.texts[cell];
          if (!text) {
            continue;
          }
          const number = isNumber(page.flags[cell] ?? 0);
          const y = originY + row * rowHeight;
          const align = number ? "right" : "left";
          if (!batchState) {
            context.font = theme.font;
            context.textBaseline = "middle";
            context.fillStyle = theme.text;
            context.textAlign = align;
          } else if (aligned !== align) {
            context.textAlign = align;
            aligned = align;
          }
          const textX = number ? x + colWidth - PADDING : x + PADDING;
          if (clipOnlyOverwide && this.#widths.fits(context, text, number, room)) {
            context.fillText(text, textX, y + rowHeight / 2);
          } else {
            context.save();
            context.beginPath();
            context.rect(x + PADDING, y, room, rowHeight);
            context.clip();
            context.fillText(text, textX, y + rowHeight / 2);
            context.restore();
            counts.clipped++;
          }
          counts.cells++;
        }
      }
    }
  }

  /** The lines between cells, as one path. */
  #lines(frame: Frame, originX: number, originY: number, right: number, bottom: number): void {
    const context = this.#context;
    const { metrics, theme, visible, scale } = frame;
    // One device pixel wide at whole-number scales, and placed on device
    // pixels at any scale, so the lines stay sharp.
    const width = Math.max(1, Math.round(scale)) / scale;
    const snap = (position: number) => Math.round(position * scale) / scale - width / 2;

    const endX = Math.min(right, originX + visible.endCol * metrics.colWidth);
    const endY = Math.min(bottom, originY + visible.endRow * metrics.rowHeight);
    context.beginPath();
    for (let row = visible.firstRow + 1; row <= visible.endRow; row++) {
      const y = snap(originY + row * metrics.rowHeight);
      context.moveTo(metrics.headerWidth, y);
      context.lineTo(endX, y);
    }
    for (let col = visible.firstCol + 1; col <= visible.endCol; col++) {
      const x = snap(originX + col * metrics.colWidth);
      context.moveTo(x, metrics.headerHeight);
      context.lineTo(x, endY);
    }
    context.lineWidth = width;
    context.strokeStyle = theme.line;
    context.stroke();
  }

  #selection(frame: Frame, originX: number, originY: number): void {
    const { metrics, theme, visible, selection } = frame;
    if (
      !selection ||
      selection.row < visible.firstRow ||
      selection.row >= visible.endRow ||
      selection.col < visible.firstCol ||
      selection.col >= visible.endCol
    ) {
      return;
    }
    const context = this.#context;
    context.lineWidth = 2;
    context.strokeStyle = theme.selection;
    // Over the grid lines that bound the cell, half inside and half outside.
    context.strokeRect(
      originX + selection.col * metrics.colWidth - 1,
      originY + selection.row * metrics.rowHeight - 1,
      metrics.colWidth,
      metrics.rowHeight,
    );
  }

  /** The strips of column names and row numbers. They cover whatever scrolled under them. */
  #headers(frame: Frame, originX: number, originY: number, right: number, bottom: number): void {
    const context = this.#context;
    const { size, metrics, theme, visible, selection, scale } = frame;
    const { rowHeight, colWidth, headerHeight, headerWidth } = metrics;

    context.fillStyle = theme.headerBackground;
    context.fillRect(0, 0, size.width, headerHeight);
    context.fillRect(0, 0, headerWidth, size.height);

    if (selection && !isEmpty(visible)) {
      context.fillStyle = theme.headerSelected;
      if (selection.col >= visible.firstCol && selection.col < visible.endCol) {
        const x = Math.max(headerWidth, originX + selection.col * colWidth);
        const end = Math.min(right, originX + (selection.col + 1) * colWidth);
        context.fillRect(x, 0, Math.max(0, end - x), headerHeight);
      }
      if (selection.row >= visible.firstRow && selection.row < visible.endRow) {
        const y = Math.max(headerHeight, originY + selection.row * rowHeight);
        const end = Math.min(bottom, originY + (selection.row + 1) * rowHeight);
        context.fillRect(0, y, headerWidth, Math.max(0, end - y));
      }
    }

    context.font = theme.font;
    context.textBaseline = "middle";
    context.textAlign = "center";
    context.fillStyle = theme.headerText;

    // A name that is partly under the other strip is clipped by it: the
    // corner is painted last.
    for (let col = visible.firstCol; col < visible.endCol; col++) {
      context.fillText(columnName(col), originX + col * colWidth + colWidth / 2, headerHeight / 2);
    }
    for (let row = visible.firstRow; row < visible.endRow; row++) {
      context.fillText(String(row + 1), headerWidth / 2, originY + row * rowHeight + rowHeight / 2);
    }

    const width = Math.max(1, Math.round(scale)) / scale;
    const snap = (position: number) => Math.round(position * scale) / scale - width / 2;
    context.beginPath();
    for (let col = visible.firstCol + 1; col <= visible.endCol; col++) {
      const x = snap(originX + col * colWidth);
      if (x > headerWidth) {
        context.moveTo(x, 0);
        context.lineTo(x, headerHeight);
      }
    }
    for (let row = visible.firstRow + 1; row <= visible.endRow; row++) {
      const y = snap(originY + row * rowHeight);
      if (y > headerHeight) {
        context.moveTo(0, y);
        context.lineTo(headerWidth, y);
      }
    }
    context.fillStyle = theme.headerBackground;
    context.fillRect(0, 0, headerWidth, headerHeight);
    context.moveTo(snap(headerWidth), 0);
    context.lineTo(snap(headerWidth), size.height);
    context.moveTo(0, snap(headerHeight));
    context.lineTo(size.width, snap(headerHeight));
    context.lineWidth = width;
    context.strokeStyle = theme.headerLine;
    context.stroke();
  }
}
