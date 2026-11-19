import { PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import { EMPTY_SPAN, type Span } from "./layout.ts";
import type { Page } from "./page.ts";

/** Pages are keyed by one number; a sheet has far fewer page columns than this. */
const KEY_STRIDE = 2 ** 20;

/**
 * The pages the grid holds: its active viewport subscription plus pinned
 * top/left/corner freeze-pane strips (`14-m1b.md` 2.5).
 */
export class PageCache {
  #pages = new Map<number, Page>();
  #span: Span = EMPTY_SPAN;
  #pinnedRowPages = 0;
  #pinnedColPages = 0;

  /** The span of main viewport pages being held. */
  get span(): Span {
    return this.#span;
  }

  /** Number of pages held (viewport + pinned freeze strips). */
  get size(): number {
    return this.#pages.size;
  }

  /**
   * Updates the active viewport span (and optional frozen row/col counts) and
   * evicts pages outside both the active span and pinned freeze strips.
   */
  retain(span: Span, frozenRows = 0, frozenCols = 0): void {
    this.#span = span;
    this.#pinnedRowPages = frozenRows > 0 ? Math.floor((frozenRows - 1) / PAGE_ROWS) + 1 : 0;
    this.#pinnedColPages = frozenCols > 0 ? Math.floor((frozenCols - 1) / PAGE_COLS) + 1 : 0;
    for (const [k, page] of this.#pages) {
      if (!this.#wants(page)) {
        this.#pages.delete(k);
      }
    }
  }

  /** Stores a page that arrived, unless it is no longer wanted. Returns whether it was kept. */
  accept(page: Page): boolean {
    if (!this.#wants(page)) {
      return false;
    }
    this.#pages.set(key(page.firstRow / PAGE_ROWS, page.firstCol / PAGE_COLS), page);
    return true;
  }

  /** The page at a position in the grid of pages, if it is held. */
  get(pageRow: number, pageCol: number): Page | undefined {
    return this.#pages.get(key(pageRow, pageCol));
  }

  /** The page that holds a cell, if it is held. */
  pageOf(row: number, col: number): Page | undefined {
    return this.get(Math.floor(row / PAGE_ROWS), Math.floor(col / PAGE_COLS));
  }

  /** Identifier of `row` from any held page covering `row`'s page strip. */
  rowIdAt(row: number): bigint | undefined {
    const pageRow = Math.floor(row / PAGE_ROWS);
    const offset = row - pageRow * PAGE_ROWS;
    for (let col = 0; col < this.#pinnedColPages; col++) {
      const page = this.get(pageRow, col);
      if (page && offset >= 0 && offset < page.rowCount) {
        return page.rowIds[offset];
      }
    }
    for (let col = this.#span.firstCol; col < this.#span.endCol; col++) {
      const page = this.get(pageRow, col);
      if (page && offset >= 0 && offset < page.rowCount) {
        return page.rowIds[offset];
      }
    }
    return undefined;
  }

  /** Identifier of `col` from any held page covering `col`'s page strip. */
  colIdAt(col: number): bigint | undefined {
    const pageCol = Math.floor(col / PAGE_COLS);
    const offset = col - pageCol * PAGE_COLS;
    for (let row = 0; row < this.#pinnedRowPages; row++) {
      const page = this.get(row, pageCol);
      if (page && offset >= 0 && offset < page.colCount) {
        return page.colIds[offset];
      }
    }
    for (let row = this.#span.firstRow; row < this.#span.endRow; row++) {
      const page = this.get(row, pageCol);
      if (page && offset >= 0 && offset < page.colCount) {
        return page.colIds[offset];
      }
    }
    return undefined;
  }

  clear(): void {
    this.#pages.clear();
    this.#span = EMPTY_SPAN;
    this.#pinnedRowPages = 0;
    this.#pinnedColPages = 0;
  }

  #wants(page: Page): boolean {
    const pageRow = page.firstRow / PAGE_ROWS;
    const pageCol = page.firstCol / PAGE_COLS;
    const span = this.#span;
    if (
      !Number.isInteger(pageRow) ||
      !Number.isInteger(pageCol) ||
      span.firstRow === span.endRow ||
      span.firstCol === span.endCol
    ) {
      return false;
    }
    if (
      pageRow >= span.firstRow &&
      pageRow < span.endRow &&
      pageCol >= span.firstCol &&
      pageCol < span.endCol
    ) {
      return true;
    }
    const inTopStrip =
      pageRow < this.#pinnedRowPages && pageCol >= span.firstCol && pageCol < span.endCol;
    const inLeftStrip =
      pageCol < this.#pinnedColPages && pageRow >= span.firstRow && pageRow < span.endRow;
    const inCorner = pageRow < this.#pinnedRowPages && pageCol < this.#pinnedColPages;
    return inTopStrip || inLeftStrip || inCorner;
  }
}

function key(pageRow: number, pageCol: number): number {
  return pageRow * KEY_STRIDE + pageCol;
}
