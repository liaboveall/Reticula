import { PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import { describe, expect, test } from "vitest";
import { PageCache } from "./cache.ts";
import type { Page } from "./page.ts";

function page(pageRow: number, pageCol: number, epoch = 1): Page {
  return {
    sheet: 0,
    epoch,
    firstRow: pageRow * PAGE_ROWS,
    firstCol: pageCol * PAGE_COLS,
    rowCount: PAGE_ROWS,
    colCount: PAGE_COLS,
    rowIds: new BigUint64Array(PAGE_ROWS),
    colIds: new BigUint64Array(PAGE_COLS),
    texts: [],
    flags: new Uint8Array(PAGE_ROWS * PAGE_COLS),
    byteLength: 0,
  };
}

const span = (firstRow: number, endRow: number, firstCol = 0, endCol = 2) => ({
  firstRow,
  endRow,
  firstCol,
  endCol,
});

describe("the page cache", () => {
  test("holds nothing and wants nothing at first", () => {
    const cache = new PageCache();
    expect(cache.size).toBe(0);
    expect(cache.accept(page(0, 0))).toBe(false);
    expect(cache.get(0, 0)).toBeUndefined();
  });

  test("keeps the pages of its span and finds them by page or by cell", () => {
    const cache = new PageCache();
    cache.retain(span(10, 13));
    const held = page(11, 1);
    expect(cache.accept(held)).toBe(true);
    expect(cache.size).toBe(1);
    expect(cache.get(11, 1)).toBe(held);
    expect(cache.pageOf(11 * PAGE_ROWS, PAGE_COLS)).toBe(held);
    expect(cache.pageOf(12 * PAGE_ROWS - 1, 2 * PAGE_COLS - 1)).toBe(held);
    expect(cache.pageOf(12 * PAGE_ROWS, PAGE_COLS)).toBeUndefined();
    expect(cache.pageOf(11 * PAGE_ROWS, 0)).toBeUndefined();
  });

  test("turns away pages outside its span", () => {
    const cache = new PageCache();
    cache.retain(span(10, 13));
    expect(cache.accept(page(9, 0))).toBe(false);
    expect(cache.accept(page(13, 0))).toBe(false);
    expect(cache.accept(page(11, 2))).toBe(false);
    expect(cache.size).toBe(0);
  });

  test("drops what falls out of the span when the span moves", () => {
    const cache = new PageCache();
    cache.retain(span(10, 13));
    for (const row of [10, 11, 12]) {
      cache.accept(page(row, 0));
    }
    cache.retain(span(12, 15));
    expect(cache.size).toBe(1);
    expect(cache.get(12, 0)).toBeDefined();
    expect(cache.get(10, 0)).toBeUndefined();
  });

  test("discards a page that arrives after its span was left", () => {
    // The engine sent this page for an earlier subscription. Since then the
    // grid has moved on, so the engine no longer counts it as held here;
    // keeping it would leave a page that is never refreshed.
    const cache = new PageCache();
    cache.retain(span(10, 13));
    cache.retain(span(500, 503));
    expect(cache.accept(page(11, 0, 1))).toBe(false);
    // Returning to the old span, the engine sends the page again.
    cache.retain(span(10, 13));
    expect(cache.accept(page(11, 0, 3))).toBe(true);
    expect(cache.get(11, 0)?.epoch).toBe(3);
  });

  test("replaces a page with a newer copy of it", () => {
    const cache = new PageCache();
    cache.retain(span(0, 1));
    cache.accept(page(0, 0, 1));
    cache.accept(page(0, 0, 2));
    expect(cache.size).toBe(1);
    expect(cache.get(0, 0)?.epoch).toBe(2);
  });

  test("ignores a page that does not start on a page boundary", () => {
    const cache = new PageCache();
    cache.retain(span(0, 4));
    expect(cache.accept({ ...page(1, 0), firstRow: PAGE_ROWS + 1 })).toBe(false);
  });

  test("forgets everything when cleared", () => {
    const cache = new PageCache();
    cache.retain(span(0, 2));
    cache.accept(page(0, 0));
    cache.clear();
    expect(cache.size).toBe(0);
    expect(cache.accept(page(0, 0))).toBe(false);
  });
});
