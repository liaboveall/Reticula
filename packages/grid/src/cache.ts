import { PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import { EMPTY_SPAN, type Span } from "./layout.ts";
import type { Page } from "./page.ts";

/** Pages are keyed by one number; a sheet has far fewer page columns than this. */
const KEY_STRIDE = 2 ** 20;

/**
 * The pages the grid holds: exactly those of its current subscription.
 *
 * The engine and the grid keep the same account of which pages the grid has
 * without ever exchanging it. The engine sends a page once when a subscription
 * starts to cover it. The grid, in turn, drops a page the moment it subscribes
 * to a range that no longer covers it, and discards a page that arrives after
 * that. So a page the engine believes the grid holds is either here or on its
 * way, and a page that returns to the range is sent again.
 */
export class PageCache {
  #pages = new Map<number, Page>();
  #span: Span = EMPTY_SPAN;

  /** The span of pages being held. */
  get span(): Span {
    return this.#span;
  }

  /** Number of pages held. */
  get size(): number {
    return this.#pages.size;
  }

  /** Changes which pages to hold and drops the ones outside the new span. */
  retain(span: Span): void {
    this.#span = span;
    for (const [key, page] of this.#pages) {
      if (!this.#wants(page)) {
        this.#pages.delete(key);
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
  }

  #wants(page: Page): boolean {
    const pageRow = page.firstRow / PAGE_ROWS;
    const pageCol = page.firstCol / PAGE_COLS;
    const span = this.#span;
    return (
      Number.isInteger(pageRow) &&
      Number.isInteger(pageCol) &&
      pageRow >= span.firstRow &&
      pageRow < span.endRow &&
      pageCol >= span.firstCol &&
      pageCol < span.endCol
    );
  }
}

function key(pageRow: number, pageCol: number): number {
  return pageRow * KEY_STRIDE + pageCol;
}
