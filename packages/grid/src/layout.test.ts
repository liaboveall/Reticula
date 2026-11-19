import { describe, expect, test } from "vitest";
import {
  cellArea,
  cellAt,
  cellBox,
  cellsOfPages,
  clampScroll,
  columnName,
  EMPTY_SPAN,
  headerHitAt,
  type Metrics,
  pagesToHold,
  type SheetLayout,
  SparseAxis,
  scrollAtThumb,
  scrollLimits,
  scrollToReveal,
  thumb,
  visibleCells,
} from "./layout.ts";

const metrics: Metrics = {
  rowHeight: 20,
  colWidth: 100,
  headerHeight: 30,
  headerWidth: 50,
  scrollbarSize: 10,
};
const sheet = { rows: 1_000_000, cols: 20 };
// A cell area of 400 by 200: four columns and ten rows.
const size = { width: 460, height: 240 };
const noMargin = { rows: 0, cols: 0 };

describe("what is visible", () => {
  test("the cell area is what the headers and scrollbars leave", () => {
    expect(cellArea(metrics, size)).toEqual({ width: 400, height: 200 });
    expect(cellArea(metrics, { width: 20, height: 20 })).toEqual({ width: 0, height: 0 });
  });

  test("at the origin, exactly the cells that fit", () => {
    expect(visibleCells(metrics, sheet, { top: 0, left: 0 }, size)).toEqual({
      firstRow: 0,
      endRow: 10,
      firstCol: 0,
      endCol: 4,
    });
  });

  test("a partly scrolled cell counts on both edges", () => {
    expect(visibleCells(metrics, sheet, { top: 30, left: 150 }, size)).toEqual({
      firstRow: 1,
      endRow: 12,
      firstCol: 1,
      endCol: 6,
    });
  });

  test("the range stops at the end of the sheet", () => {
    const small = { rows: 3, cols: 2 };
    expect(visibleCells(metrics, small, { top: 0, left: 0 }, size)).toEqual({
      firstRow: 0,
      endRow: 3,
      firstCol: 0,
      endCol: 2,
    });
  });

  test("nothing is visible without a sheet or without room", () => {
    expect(visibleCells(metrics, { rows: 0, cols: 0 }, { top: 0, left: 0 }, size)).toBe(EMPTY_SPAN);
    expect(visibleCells(metrics, sheet, { top: 0, left: 0 }, { width: 40, height: 40 })).toBe(
      EMPTY_SPAN,
    );
  });
});

describe("scrolling", () => {
  test("ends with the last row and column fully in view", () => {
    const limits = scrollLimits(metrics, sheet, size);
    expect(limits).toEqual({ top: 1_000_000 * 20 - 200, left: 20 * 100 - 400 });
    const visible = visibleCells(metrics, sheet, limits, size);
    expect(visible.endRow).toBe(sheet.rows);
    expect(visible.firstRow).toBe(sheet.rows - 10);
    expect(visible.endCol).toBe(sheet.cols);
  });

  test("has no range when everything fits", () => {
    expect(scrollLimits(metrics, { rows: 5, cols: 2 }, size)).toEqual({ top: 0, left: 0 });
  });

  test("is kept within its limits", () => {
    const limits = { top: 1000, left: 300 };
    expect(clampScroll({ top: -5, left: 400 }, limits)).toEqual({ top: 0, left: 300 });
    expect(clampScroll({ top: 2000, left: -1 }, limits)).toEqual({ top: 1000, left: 0 });
    expect(clampScroll({ top: 12.5, left: 7 }, limits)).toEqual({ top: 12.5, left: 7 });
  });
});

describe("what a point is over", () => {
  const at = (x: number, y: number, scroll = { top: 0, left: 0 }) =>
    cellAt(metrics, sheet, scroll, size, x, y);

  test("the first cell starts right after the headers", () => {
    expect(at(50, 30)).toEqual({ row: 0, col: 0 });
    expect(at(149, 49)).toEqual({ row: 0, col: 0 });
    expect(at(150, 50)).toEqual({ row: 1, col: 1 });
  });

  test("the headers and the scrollbars are not cells", () => {
    expect(at(49, 100)).toBeNull();
    expect(at(100, 29)).toBeNull();
    expect(at(450, 100)).toBeNull();
    expect(at(100, 230)).toBeNull();
  });

  test("scrolling shifts the cell under a point", () => {
    expect(at(50, 30, { top: 30, left: 150 })).toEqual({ row: 1, col: 1 });
    expect(at(101, 41, { top: 30, left: 150 })).toEqual({ row: 2, col: 2 });
  });

  test("the space past the last row or column is not a cell", () => {
    const small = { rows: 3, cols: 2 };
    expect(cellAt(metrics, small, { top: 0, left: 0 }, size, 60, 100)).toBeNull();
    expect(cellAt(metrics, small, { top: 0, left: 0 }, size, 260, 40)).toBeNull();
    expect(cellAt(metrics, small, { top: 0, left: 0 }, size, 249, 89)).toEqual({ row: 2, col: 1 });
  });
});

describe("the pages to hold", () => {
  test("are the pages the visible cells are in", () => {
    // Rows 100 to 199 are in page rows 1 to 3; columns 0 to 11 in page column 0.
    const visible = { firstRow: 100, endRow: 200, firstCol: 0, endCol: 12 };
    expect(pagesToHold(visible, sheet, noMargin)).toEqual({
      firstRow: 1,
      endRow: 4,
      firstCol: 0,
      endCol: 1,
    });
  });

  test("grow by a margin in multiples of the visible range", () => {
    // 40 rows visible and a margin of two: 80 more rows on each side.
    const visible = { firstRow: 1000, endRow: 1040, firstCol: 4, endCol: 16 };
    expect(pagesToHold(visible, sheet, { rows: 2, cols: 1 })).toEqual({
      firstRow: Math.floor(920 / 64),
      endRow: Math.floor(1119 / 64) + 1,
      firstCol: 0,
      endCol: 2,
    });
  });

  test("stop at the edges of the sheet", () => {
    const top = { firstRow: 0, endRow: 40, firstCol: 0, endCol: 12 };
    expect(pagesToHold(top, sheet, { rows: 2, cols: 1 }).firstRow).toBe(0);
    const bottom = { firstRow: 999_960, endRow: 1_000_000, firstCol: 8, endCol: 20 };
    expect(pagesToHold(bottom, sheet, { rows: 2, cols: 1 })).toEqual({
      firstRow: Math.floor(999_880 / 64),
      endRow: 15_625,
      firstCol: 0,
      endCol: 2,
    });
  });

  test("are bounded however much is visible", () => {
    const huge = { rows: 10_000_000, cols: 10_000 };
    const visible = { firstRow: 640, endRow: 100_000, firstCol: 0, endCol: 5000 };
    expect(pagesToHold(visible, huge, noMargin)).toEqual({
      firstRow: 10,
      endRow: 74,
      firstCol: 0,
      endCol: 16,
    });
  });

  test("are none when nothing is visible", () => {
    expect(pagesToHold(EMPTY_SPAN, sheet, { rows: 2, cols: 1 })).toBe(EMPTY_SPAN);
  });

  test("hold whole pages of cells, cut off at the end of the sheet", () => {
    expect(cellsOfPages({ firstRow: 1, endRow: 4, firstCol: 0, endCol: 2 }, sheet)).toEqual({
      firstRow: 64,
      endRow: 256,
      firstCol: 0,
      endCol: 20,
    });
    expect(
      cellsOfPages({ firstRow: 15_624, endRow: 15_625, firstCol: 1, endCol: 2 }, sheet),
    ).toEqual({ firstRow: 999_936, endRow: 1_000_000, firstCol: 16, endCol: 20 });
    expect(cellsOfPages(EMPTY_SPAN, sheet)).toBe(EMPTY_SPAN);
  });
});

describe("a scrollbar thumb", () => {
  test("is as long as the visible share of the content", () => {
    expect(thumb(200, 1000, 250, 0, 10)).toEqual({ offset: 0, length: 50 });
  });

  test("travels the track as the content scrolls", () => {
    expect(thumb(200, 1000, 250, 375, 10)).toEqual({ offset: 75, length: 50 });
    expect(thumb(200, 1000, 250, 750, 10)).toEqual({ offset: 150, length: 50 });
  });

  test("never gets too short to grab", () => {
    // A million rows: the proportional thumb would be a hundredth of a pixel.
    const placed = thumb(600, 24_000_000, 600, 12_000_000, 32);
    expect(placed?.length).toBe(32);
    expect(placed?.offset).toBeCloseTo((600 - 32) / 2, 1);
  });

  test("is absent when there is nothing to scroll", () => {
    expect(thumb(200, 100, 250, 0, 10)).toBeNull();
    expect(thumb(200, 250, 250, 0, 10)).toBeNull();
    expect(thumb(0, 1000, 250, 0, 10)).toBeNull();
  });

  test("maps back to the scroll position it shows", () => {
    for (const scroll of [0, 1, 4_999_999.5, 12_000_000, 23_999_400]) {
      const placed = thumb(600, 24_000_000, 600, scroll, 32);
      expect(placed).not.toBeNull();
      if (placed) {
        expect(scrollAtThumb(600, 24_000_000, 600, placed.offset, placed.length)).toBeCloseTo(
          scroll,
          6,
        );
      }
    }
  });

  test("dragged past either end of the track stops at the end", () => {
    expect(scrollAtThumb(200, 1000, 250, -40, 50)).toBe(0);
    expect(scrollAtThumb(200, 1000, 250, 900, 50)).toBe(750);
  });
});

describe("column names", () => {
  test("run from A to Z and then take another letter", () => {
    expect([0, 1, 25, 26, 27, 51, 52, 701, 702, 16_383].map(columnName)).toEqual([
      "A",
      "B",
      "Z",
      "AA",
      "AB",
      "AZ",
      "BA",
      "ZZ",
      "AAA",
      "XFD",
    ]);
  });
});

describe("where a cell is", () => {
  test("the first cell sits right after the headers", () => {
    expect(cellBox(metrics, { top: 0, left: 0 }, 0, 0)).toEqual({
      left: 50,
      top: 30,
      width: 100,
      height: 20,
    });
  });

  test("scrolling moves it, out of the area if far enough", () => {
    expect(cellBox(metrics, { top: 30, left: 150 }, 2, 3)).toEqual({
      left: 50 + 300 - 150,
      top: 30 + 40 - 30,
      width: 100,
      height: 20,
    });
    expect(cellBox(metrics, { top: 500, left: 0 }, 0, 0).top).toBe(30 - 500);
  });

  test("it is the cell a point inside it is over", () => {
    const scroll = { top: 130, left: 250 };
    const box = cellBox(metrics, scroll, 9, 4);
    expect(cellAt(metrics, sheet, scroll, size, box.left + 1, box.top + 1)).toEqual({
      row: 9,
      col: 4,
    });
  });
});

describe("bringing a cell into view", () => {
  // The cell area is 400 by 200: columns 0 to 3 and rows 0 to 9 at the origin.
  const reveal = (scroll: { top: number; left: number }, row: number, col: number) =>
    scrollToReveal(metrics, size, scroll, row, col);

  test("does nothing when the cell is fully visible", () => {
    expect(reveal({ top: 0, left: 0 }, 9, 3)).toEqual({ top: 0, left: 0 });
    expect(reveal({ top: 35, left: 120 }, 5, 3)).toEqual({ top: 35, left: 120 });
  });

  test("scrolls down or right just far enough to show the whole cell", () => {
    expect(reveal({ top: 0, left: 0 }, 10, 0)).toEqual({ top: 20, left: 0 });
    expect(reveal({ top: 0, left: 0 }, 0, 4)).toEqual({ top: 0, left: 100 });
    expect(reveal({ top: 0, left: 0 }, 500, 19)).toEqual({ top: 501 * 20 - 200, left: 1600 });
  });

  test("scrolls up or left to the start of the cell", () => {
    expect(reveal({ top: 1000, left: 800 }, 49, 7)).toEqual({ top: 980, left: 700 });
    expect(reveal({ top: 1000, left: 800 }, 0, 0)).toEqual({ top: 0, left: 0 });
  });

  test("completes a cell that is cut off at an edge", () => {
    // Row 1 starts at 20; scrolled to 30 it is half hidden at the top.
    expect(reveal({ top: 30, left: 0 }, 1, 0)).toEqual({ top: 20, left: 0 });
    // Row 11 ends at 240; the area ends at 230.
    expect(reveal({ top: 30, left: 0 }, 11, 0)).toEqual({ top: 40, left: 0 });
  });

  test("a cell larger than the area is aligned at its start", () => {
    const tall = { ...metrics, rowHeight: 500 };
    expect(scrollToReveal(tall, size, { top: 0, left: 0 }, 3, 0)).toEqual({ top: 1500, left: 0 });
  });
});

describe("sparse axis metrics and frozen panes", () => {
  test("SparseAxis matches naive cumulative offsets and inverse lookup", () => {
    const custom: [number, number][] = [
      [2, 55],
      [3, 16],
      [10, 80],
    ];
    const axis = new SparseAxis(20, custom);
    const customMap = new Map(custom);
    let expectedOffset = 0;
    for (let pos = 0; pos < 25; pos++) {
      const sz = customMap.get(pos) ?? 20;
      expect(axis.offsetOf(pos)).toBe(expectedOffset);
      expect(axis.sizeOf(pos)).toBe(sz);
      for (let px = expectedOffset; px < expectedOffset + sz; px += 7) {
        expect(axis.indexAtOffset(px)).toBe(pos);
      }
      expectedOffset += sz;
    }
    expect(axis.totalSize(25)).toBe(expectedOffset);
  });

  test("frozen rows and columns pin top/left coordinates and skip scrollToReveal", () => {
    const layout: SheetLayout = {
      rowAxis: new SparseAxis(20, [[0, 30]]),
      colAxis: new SparseAxis(100, [[0, 140]]),
      frozenRows: 1,
      frozenCols: 1,
    };
    const scroll = { top: 400, left: 300 };
    // Pinned corner (0, 0) stays at (headerWidth, headerHeight) regardless of scroll!
    expect(cellBox(metrics, scroll, 0, 0, size, sheet, layout)).toEqual({
      left: 50,
      top: 30,
      width: 140,
      height: 30,
    });
    expect(cellAt(metrics, sheet, scroll, size, 60, 40, layout)).toEqual({ row: 0, col: 0 });
    // Point below frozen row 0 (`y = 30 + 35 = 65`) hits scrolled row (`400 + 35 = 435` -> row 21).
    expect(cellAt(metrics, sheet, scroll, size, 60, 65, layout)).toEqual({ row: 21, col: 0 });
    // Revealing a frozen row/col leaves scroll untouched.
    expect(scrollToReveal(metrics, size, scroll, 0, 0, sheet, layout)).toEqual(scroll);
  });

  test("headerHitAt distinguishes corner, headers, and resize dividers", () => {
    expect(headerHitAt(metrics, sheet, { top: 0, left: 0 }, size, 25, 15)).toEqual({
      kind: "corner",
    });
    expect(headerHitAt(metrics, sheet, { top: 0, left: 0 }, size, 90, 15)).toEqual({
      kind: "col",
      col: 0,
    });
    expect(headerHitAt(metrics, sheet, { top: 0, left: 0 }, size, 149, 15)).toEqual({
      kind: "colResize",
      col: 0,
    });
    expect(headerHitAt(metrics, sheet, { top: 0, left: 0 }, size, 25, 49)).toEqual({
      kind: "rowResize",
      row: 0,
    });
  });
});
