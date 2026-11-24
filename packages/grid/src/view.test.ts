import { describe, expect, test } from "vitest";
import { FilterRowMap, filterButtonBox } from "./filter.ts";
import {
  cellArea,
  cellAt,
  cellBox,
  clampScroll,
  cornerOf,
  DEFAULT_METRICS,
  editorBox,
  frozenPixels,
  headerHitAt,
  isOverFillHandle,
  labelStep,
  type Metrics,
  middleOf,
  type Point,
  pointOfCell,
  reanchor,
  type Size,
  SparseAxis,
  scrollLimits,
  scrollToCorner,
  type View,
  viewMetrics,
  visibleCells,
} from "./layout.ts";
import { ZOOM_STEPS } from "./zoom.ts";

const DENSITIES = [1, 1.1, 1.25, 1.5, 2];
const SHEET = { rows: 1_000_000, cols: 2_000 };

type Sizes = readonly (readonly [number, number])[];

const CUSTOM_ROWS: Sizes = [
  [1, 41],
  [2, 16],
  [3, 5],
  [7, 600],
  [499_990, 33],
  [500_002, 17],
  [500_003, 120],
];
const CUSTOM_COLS: Sizes = [
  [0, 64],
  [2, 24],
  [3, 7],
  [5, 300],
  [1_001, 35],
];

interface Setting {
  readonly zoom?: number;
  readonly scale?: number;
  readonly width?: number;
  readonly height?: number;
  readonly customRows?: Sizes;
  readonly customCols?: Sizes;
  readonly frozenRows?: number;
  readonly frozenCols?: number;
}

/** A view as the grid has it: everything in it is a whole number of device pixels. */
function viewOf(setting: Setting, scroll = { top: 0, left: 0 }): View {
  const { zoom = 1, scale = 1, width = 1180, height = 640 } = setting;
  const size: Size = {
    width: Math.round(width * scale) / scale,
    height: Math.round(height * scale) / scale,
  };
  const metrics = viewMetrics(DEFAULT_METRICS, zoom, scale);
  const layout = {
    rowAxis: new SparseAxis(DEFAULT_METRICS.rowHeight, setting.customRows ?? [], scale, zoom),
    colAxis: new SparseAxis(DEFAULT_METRICS.colWidth, setting.customCols ?? [], scale, zoom),
    frozenRows: setting.frozenRows ?? 0,
    frozenCols: setting.frozenCols ?? 0,
  };
  const limits = scrollLimits(metrics, SHEET, size, layout);
  const snap = (position: number) => Math.round(position * scale) / scale;
  const within = clampScroll({ top: snap(scroll.top), left: snap(scroll.left) }, limits);
  return { metrics, sheet: SHEET, size, layout, scroll: within };
}

/** The same, scrolled to where a row and a column are in the corner. */
function viewAt(setting: Setting, row: number, col: number): View {
  const view = viewOf(setting);
  const corner = { row: { index: row, fraction: 0 }, col: { index: col, fraction: 0 } };
  return { ...view, scroll: scrollToCorner(view, corner) };
}

const visibleIn = (view: View) =>
  visibleCells(view.metrics, view.sheet, view.scroll, view.size, view.layout);

const boxIn = (view: View, row: number, col: number) =>
  cellBox(view.metrics, view.scroll, row, col, view.size, view.sheet, view.layout);

const cellIn = (view: View, { x, y }: Point) =>
  cellAt(view.metrics, view.sheet, view.scroll, view.size, x, y, view.layout);

const isWhole = (css: number, scale: number) =>
  Math.abs(css * scale - Math.round(css * scale)) < 1e-6;

describe("the sizes on the screen", () => {
  test.each(DENSITIES)("are whole device pixels at %f, and at least one", (scale) => {
    for (const zoom of ZOOM_STEPS) {
      const metrics = viewMetrics(DEFAULT_METRICS, zoom, scale);
      for (const size of Object.values(metrics)) {
        expect(isWhole(size, scale)).toBe(true);
        expect(size * scale).toBeGreaterThanOrEqual(1);
      }
      const rows = new SparseAxis(DEFAULT_METRICS.rowHeight, [[2, 5]], scale, zoom);
      expect(rows.defaultSize).toBe(metrics.rowHeight);
      expect(rows.offsetOf(2)).toBeCloseTo(2 * metrics.rowHeight, 9);
      expect(rows.sizeOf(2) * scale).toBeGreaterThanOrEqual(1);
    }
    const tiny = viewMetrics({ ...DEFAULT_METRICS, rowHeight: 1 }, 0.25, scale);
    expect(tiny.rowHeight * scale).toBeCloseTo(1, 9);
  });

  test("are the workbook's own at a zoom of 1 and quarter steps of density", () => {
    for (const scale of [1, 1.25, 1.5, 1.75, 2]) {
      expect(viewMetrics(DEFAULT_METRICS, 1, scale)).toEqual(DEFAULT_METRICS);
    }
  });

  test("of the headers change less than those of the cells, and of scrollbars not at all", () => {
    const at = (zoom: number): Metrics => viewMetrics(DEFAULT_METRICS, zoom, 1);
    expect(at(0.25)).toEqual({
      rowHeight: 6,
      colWidth: 28,
      headerHeight: 18,
      headerWidth: 48,
      scrollbarSize: 12,
    });
    expect(at(2)).toEqual({
      rowHeight: 48,
      colWidth: 224,
      headerHeight: 36,
      headerWidth: 96,
      scrollbarSize: 12,
    });
    expect(at(1.25).headerHeight).toBe(30);
  });

  test("of what is hidden are nothing", () => {
    const axis = new SparseAxis(24, [[3, 0]], 1.25, 0.25);
    expect(axis.sizeOf(3)).toBe(0);
    expect(axis.offsetOf(4)).toBe(axis.offsetOf(3));
    expect(axis.indexAtOffset(axis.offsetOf(4))).toBe(4);
  });
});

/** Every combination of zoom and density, and of what a sheet may have. */
const settings = (): Setting[] =>
  ZOOM_STEPS.flatMap((zoom) =>
    DENSITIES.flatMap((scale) => [
      { zoom, scale },
      { zoom, scale, customRows: CUSTOM_ROWS, customCols: CUSTOM_COLS },
      {
        zoom,
        scale,
        customRows: CUSTOM_ROWS,
        customCols: CUSTOM_COLS,
        frozenRows: 3,
        frozenCols: 1,
      },
    ]),
  );

describe("the pointer", () => {
  const places = [
    [0, 0],
    [2, 1],
    [499_988, 1_000],
    [999_990, 1_990],
  ] as const;

  test("in the middle of a cell is over that cell, at every zoom and density", () => {
    for (const setting of settings()) {
      for (const [row, col] of places) {
        const view = viewAt(setting, row, col);
        const visible = visibleIn(view);
        const frozen = frozenPixels(view.metrics, view.sheet, view.size, view.layout);
        const { metrics } = view;
        const area = cellArea(metrics, view.size);
        const right = metrics.headerWidth + area.width;
        const bottom = metrics.headerHeight + area.height;
        const rows = [visible.firstRow, visible.firstRow + 1, visible.firstRow + 3];
        rows.push(visible.endRow - 2, ...Array.from({ length: frozen.frozenRows }, (_, i) => i));
        const cols = [visible.firstCol, visible.firstCol + 1, visible.firstCol + 2];
        cols.push(visible.endCol - 2, ...Array.from({ length: frozen.frozenCols }, (_, i) => i));
        for (const cellRow of new Set(rows)) {
          for (const cellCol of new Set(cols)) {
            const box = boxIn(view, cellRow, cellCol);
            const point = { x: box.left + box.width / 2, y: box.top + box.height / 2 };
            const under =
              (cellRow >= frozen.frozenRows && point.y < metrics.headerHeight + frozen.height) ||
              (cellCol >= frozen.frozenCols && point.x < metrics.headerWidth + frozen.width);
            if (under || point.x >= right || point.y >= bottom) {
              continue;
            }
            const found = cellIn(view, point);
            if (found?.row !== cellRow || found.col !== cellCol) {
              expect({ setting, cellRow, cellCol, found }).toBe("the cell");
            }
            // Selected, it does not turn into its own fill handle, nor do those around it.
            for (const range of [
              { firstRow: cellRow, endRow: cellRow + 1, firstCol: cellCol, endCol: cellCol + 1 },
              { firstRow: 0, endRow: cellRow, firstCol: 0, endCol: cellCol },
              { firstRow: 0, endRow: cellRow, firstCol: 0, endCol: cellCol + 1 },
            ]) {
              if (isOverFillHandle(view, range, point)) {
                expect({ setting, cellRow, cellCol, range }).toBe("the cell");
              }
            }
          }
        }
      }
    }
  });

  test("in the middle half of a header is over that header, at every zoom and density", () => {
    for (const setting of settings()) {
      const { scale = 1 } = setting;
      for (const [row, col] of places) {
        const view = viewAt(setting, row, col);
        const { metrics, sheet, scroll, size, layout } = view;
        const hit = (x: number, y: number) =>
          headerHitAt(metrics, sheet, scroll, size, x, y, layout);
        const visible = visibleIn(view);
        const frozen = frozenPixels(metrics, sheet, size, layout);
        const left = metrics.headerWidth + frozen.width;
        const top = metrics.headerHeight + frozen.height;
        for (const part of [0.251, 0.5, 0.749]) {
          for (let at = 0; at < 8; at++) {
            const header = { row: visible.firstRow + at, col: visible.firstCol + at };
            const box = boxIn(view, header.row, header.col);
            const x = box.left + box.width * part;
            const y = box.top + box.height * part;
            // A header that is a pixel high has no halves to point at.
            if (
              box.height * scale >= 2 &&
              y >= top &&
              y < size.height - metrics.scrollbarSize &&
              header.row < sheet.rows
            ) {
              expect(hit(4, y)).toEqual({ kind: "row", row: header.row });
            }
            if (
              box.width * scale >= 2 &&
              x >= left &&
              x < size.width - metrics.scrollbarSize &&
              header.col < sheet.cols
            ) {
              expect(hit(x, 4)).toEqual({ kind: "col", col: header.col });
            }
          }
        }
      }
    }
  });

  test("grabs an edge next to it, from headers that are large enough", () => {
    for (const zoom of ZOOM_STEPS) {
      const view = viewAt({ zoom }, 100, 10);
      const { metrics, sheet, scroll, size, layout } = view;
      const box = boxIn(view, 104, 13);
      const bottom = box.top + box.height;
      const at = (y: number) => headerHitAt(metrics, sheet, scroll, size, 4, y, layout);
      const isGrabbed = metrics.rowHeight >= 10;
      expect(at(bottom - 1)).toEqual({ kind: isGrabbed ? "rowResize" : "row", row: 104 });
      expect(at(bottom + 1)).toEqual({
        kind: isGrabbed ? "rowResize" : "row",
        row: isGrabbed ? 104 : 105,
      });
      const right = box.left + box.width;
      expect(headerHitAt(metrics, sheet, scroll, size, right - 2, 4, layout)).toEqual({
        kind: "colResize",
        col: 13,
      });
    }
  });

  test("grabs the edge of a narrow column from the wide one next to it", () => {
    const view = viewOf({ customCols: [[1, 6]] });
    const { metrics, sheet, scroll, size, layout } = view;
    const right = boxIn(view, 0, 1).left + 6;
    const at = (x: number) => headerHitAt(metrics, sheet, scroll, size, x, 4, layout);
    expect(at(right + 2)).toEqual({ kind: "colResize", col: 1 });
    expect(at(right - 1)).toEqual({ kind: "col", col: 1 });
    expect(at(right - 6 - 2)).toEqual({ kind: "colResize", col: 0 });
  });

  test("grabs the fill handle around its corner, and not over the headers", () => {
    for (const zoom of ZOOM_STEPS) {
      const view = viewAt({ zoom }, 100, 10);
      const box = boxIn(view, 105, 12);
      const corner = { x: box.left + box.width, y: box.top + box.height };
      const range = { firstRow: 103, endRow: 106, firstCol: 11, endCol: 13 };
      const near = Math.min(8, box.height / 4) - 0.5;
      for (const [dx, dy] of [
        [-1, -1],
        [1, -1],
        [-1, 1],
        [1, 1],
      ] as const) {
        const point = { x: corner.x + dx, y: corner.y + dy * Math.min(1, near) };
        expect(isOverFillHandle(view, range, point)).toBe(true);
        expect(isOverFillHandle(view, range, { ...point, x: corner.x + dx * 9 })).toBe(false);
      }
      const above = { firstRow: 90, endRow: 100, firstCol: 11, endCol: 13 };
      const atHeader = { x: corner.x, y: view.metrics.headerHeight - 1 };
      expect(isOverFillHandle(view, above, atHeader)).toBe(false);
    }
  });

  test("is in the device pixel it points into", () => {
    const axis = new SparseAxis(24, [], 1.5, 0.25);
    expect(axis.sizeOf(0)).toBe(6);
    // Nine device pixels a row.
    expect(axis.indexAtOffset(5.99)).toBe(0);
    expect(axis.indexAtOffset(6)).toBe(1);
    expect(axis.endAtOffset(6)).toBe(1);
    expect(axis.endAtOffset(6.01)).toBe(2);
  });
});

describe("what is at a point stays there", () => {
  const starts = [
    [0, 0],
    [4, 3],
    [500_000, 1_000],
    [900_000, 1_500],
  ] as const;

  test.each([1, 1.1, 1.25, 1.5, 2])("when the zoom changes, at %f", (scale) => {
    const tolerance = 1 / scale + 1e-6;
    for (const custom of [false, true]) {
      const sizes = custom ? { customRows: CUSTOM_ROWS, customCols: CUSTOM_COLS } : {};
      for (const from of ZOOM_STEPS) {
        for (const to of ZOOM_STEPS) {
          for (const [row, col] of starts) {
            const before = viewAt({ zoom: from, scale, ...sizes }, row, col);
            const after = viewOf({ zoom: to, scale, ...sizes });
            for (const point of [
              { x: 613, y: 333 },
              { x: 200, y: 120 },
            ]) {
              const scroll = reanchor(before, after, point);
              const cell = cellIn(before, point);
              if (!cell) {
                expect(cell).not.toBeNull();
                continue;
              }
              // Where the point is in its cell, before and after, as far as one can scroll.
              const view = { ...after, scroll };
              const limits = scrollLimits(after.metrics, SHEET, after.size, after.layout);
              const was = boxIn(before, cell.row, cell.col);
              const is = boxIn(view, cell.row, cell.col);
              const part = {
                x: (point.x - was.left) / was.width,
                y: (point.y - was.top) / was.height,
              };
              if (scroll.top > 0 && scroll.top < limits.top) {
                expect(Math.abs(is.top + part.y * is.height - point.y)).toBeLessThan(tolerance);
              }
              if (scroll.left > 0 && scroll.left < limits.left) {
                expect(Math.abs(is.left + part.x * is.width - point.x)).toBeLessThan(tolerance);
              }
            }
          }
        }
      }
    }
  });

  test("through a long gesture there and back", () => {
    let view = viewAt({ zoom: 1 }, 500_000, 1_000);
    const point = { x: 700, y: 401 };
    const was = cellIn(view, point);
    const zooms = [];
    for (let step = 20; step >= 5; step--) {
      zooms.push(step / 20);
    }
    for (const zoom of [...zooms, ...zooms.toReversed()]) {
      const next = viewOf({ zoom });
      view = { ...next, scroll: reanchor(view, next, point) };
    }
    expect(cellIn(view, point)).toEqual(was);
    expect(Math.abs(view.scroll.top - viewAt({ zoom: 1 }, 500_000, 1_000).scroll.top)).toBeLessThan(
      1e-3,
    );
  });

  test.each([1, 1.1, 1.25, 1.5, 2])(
    "in the first row and column when the density was %f",
    (from) => {
      for (const to of DENSITIES) {
        for (const zoom of ZOOM_STEPS) {
          for (const frozen of [0, 2]) {
            const setting = {
              zoom,
              customRows: CUSTOM_ROWS,
              customCols: CUSTOM_COLS,
              frozenRows: frozen,
              frozenCols: frozen,
            };
            for (const [row, col] of [...starts, [500_002, 1_001] as const]) {
              for (const part of [0, 0.4, 0.999]) {
                const still = viewOf({ ...setting, scale: from });
                const scroll = scrollToCorner(still, {
                  row: { index: Math.max(frozen, row), fraction: part },
                  col: { index: Math.max(frozen, col), fraction: part },
                });
                const before = { ...still, scroll };
                // The same window, which at another density has other CSS pixels.
                const after = viewOf({
                  ...setting,
                  scale: to,
                  width: (1180 * from) / to,
                  height: (640 * from) / to,
                });
                const moved = { ...after, scroll: reanchor(before, after, null) };
                const limits = scrollLimits(after.metrics, SHEET, after.size, after.layout);
                if (moved.scroll.top < limits.top && moved.scroll.left < limits.left) {
                  const first = visibleIn(before);
                  const now = visibleIn(moved);
                  if (now.firstRow !== first.firstRow || now.firstCol !== first.firstCol) {
                    expect({ from, to, zoom, frozen, part, first, now }).toBe(
                      "the same cell first",
                    );
                  }
                }
                expect(isWhole(moved.scroll.top, to)).toBe(true);
              }
            }
          }
        }
      }
    },
  );

  test("and over the headers or frozen cells that is what scrolls first", () => {
    const setting = { frozenRows: 2, frozenCols: 1 };
    const before = viewAt({ ...setting, zoom: 1 }, 500_000, 1_000);
    for (const zoom of [0.25, 0.5, 2]) {
      const after = viewOf({ ...setting, zoom });
      for (const point of [
        { x: 10, y: 10 },
        { x: 100, y: 40 },
        { x: -5, y: 70 },
      ]) {
        const corner = cornerOf({ ...after, scroll: reanchor(before, after, point) });
        expect(corner.row).toEqual({ index: 500_000, fraction: 0 });
        expect(corner.col).toEqual({ index: 1_000, fraction: 0 });
      }
      // Along one axis only, if it is frozen cells along that one.
      const half = { ...after, scroll: reanchor(before, after, { x: 100, y: 400 }) };
      expect(cornerOf(half).col).toEqual({ index: 1_000, fraction: 0 });
      const was = cellIn(before, { x: 100, y: 400 });
      const is = cellIn(half, { x: 100, y: 400 });
      expect(is?.row).toBe(was?.row);
    }
  });

  test("under frozen cells that grow over the point, it comes out below them", () => {
    const setting = { frozenRows: 6, frozenCols: 0 };
    const before = viewAt({ ...setting, zoom: 0.5 }, 500_000, 0);
    const point = { x: 500, y: 120 };
    const cell = cellIn(before, point);
    const after = viewOf({ ...setting, zoom: 2 });
    const view = { ...after, scroll: reanchor(before, after, point) };
    expect(visibleIn(view).firstRow).toBe(cell?.row);
  });

  test("when there comes to be no room for the frozen cells, and when there is again", () => {
    const setting = { frozenRows: 8, frozenCols: 2, height: 330, width: 560 };
    const small = viewAt({ ...setting, zoom: 0.5 }, 500_000, 1_000);
    expect(frozenPixels(small.metrics, SHEET, small.size, small.layout).frozenRows).toBe(8);
    const large = viewOf({ ...setting, zoom: 2 });
    expect(frozenPixels(large.metrics, SHEET, large.size, large.layout).frozenRows).toBe(0);
    const point = { x: 500, y: 250 };
    const cell = cellIn(small, point);
    const zoomedIn = { ...large, scroll: reanchor(small, large, point) };
    expect(cellIn(zoomedIn, point)).toEqual(cell);
    const back = viewOf({ ...setting, zoom: 0.5 });
    const zoomedOut = { ...back, scroll: reanchor(zoomedIn, back, point) };
    expect(cellIn(zoomedOut, point)).toEqual(cell);
    // At the start of the sheet there is nothing that could be scrolled to before it.
    const start = viewOf({ ...setting, zoom: 2 });
    expect(reanchor(start, back, null)).toEqual({ top: 0, left: 0 });
  });

  test("which may be a cell, as far as it is in view, or the middle", () => {
    const view = viewAt({ frozenRows: 2, frozenCols: 1 }, 500_000, 1_000);
    const box = boxIn(view, 500_004, 1_002);
    expect(pointOfCell(view, { row: 500_004, col: 1_002 })).toEqual({
      x: box.left + box.width / 2,
      y: box.top + box.height / 2,
    });
    const { metrics, size } = view;
    const area = cellArea(metrics, size);
    const middle = {
      x: metrics.headerWidth + (112 + area.width) / 2,
      y: metrics.headerHeight + (48 + area.height) / 2,
    };
    expect(middleOf(view)).toEqual(middle);
    expect(pointOfCell(view, { row: 3, col: 1_002 })).toBeNull();
    expect(pointOfCell(view, { row: 500_004, col: 3 })).toBeNull();
    // Of a cell that is cut off, the part that is in view.
    const { endCol } = visibleIn(view);
    for (const cell of [
      { row: 500_004, col: endCol - 1 },
      { row: 1, col: 0 },
      { row: 500_000, col: 0 },
    ]) {
      const point = pointOfCell(view, cell);
      expect(point && cellIn(view, point)).toEqual(cell);
    }
  });

  test("a corner is found again, in a window of another size too", () => {
    const setting = { customRows: CUSTOM_ROWS, frozenRows: 1 };
    const view = viewAt(setting, 500_002, 1_000);
    const scrolled = { ...view, scroll: { ...view.scroll, top: view.scroll.top + 9 } };
    const corner = cornerOf(scrolled);
    expect(corner.row).toEqual({ index: 500_002, fraction: 9 / 17 });
    const other = viewOf({ ...setting, zoom: 0.5, scale: 1.5, width: 700, height: 500 });
    const again = cornerOf({ ...other, scroll: scrollToCorner(other, corner) });
    expect(again.row.index).toBe(500_002);
    expect(again.col.index).toBe(1_000);
    expect(scrollToCorner(view, corner)).toEqual(scrolled.scroll);
  });
});

describe("a cell's editor", () => {
  const LEAST = 11 / 13;

  test("is as large as the cell where the text can be read", () => {
    for (const zoom of [0.9, 1, 1.5, 2]) {
      const view = viewOf({ zoom });
      const cell = boxIn(view, 4, 3);
      for (const shown of [false, true]) {
        const editor = editorBox(view.metrics, view.size, cell, zoom, LEAST, 1, shown);
        expect(editor).toMatchObject({ ...cell, zoom });
      }
    }
  });

  test.each(DENSITIES)("is larger where it could not, inside the cells' area, at %f", (scale) => {
    for (const zoom of ZOOM_STEPS.filter((step) => step < LEAST)) {
      const view = viewAt({ zoom, scale }, 100, 10);
      const { metrics, size } = view;
      const area = cellArea(metrics, size);
      const { firstRow, endRow, firstCol, endCol } = visibleIn(view);
      for (const row of [firstRow, firstRow + 1, 110, endRow - 2]) {
        for (const col of [firstCol, firstCol + 1, 14, endCol - 2]) {
          const cell = boxIn(view, row, col);
          const hidden = editorBox(metrics, size, cell, zoom, LEAST, scale, false);
          const shown = editorBox(metrics, size, cell, zoom, LEAST, scale, true);
          // A composition begins in the hidden editor and is seen in the shown one.
          for (const side of ["left", "top", "width", "height", "maxWidth"] as const) {
            expect(shown[side]).toBeCloseTo(hidden[side], 6);
          }
          expect(shown.zoom * 13).toBeGreaterThanOrEqual(11 - 1e-9);
          expect(shown.height).toBeGreaterThan(cell.height);
          expect(shown.height).toBeGreaterThanOrEqual(shown.zoom * 13 * 1.2);
          for (const position of [shown.left, shown.top, shown.width, shown.height]) {
            expect(isWhole(position, scale)).toBe(true);
          }
          expect(shown.left).toBeGreaterThanOrEqual(metrics.headerWidth - 1e-6);
          expect(shown.top).toBeGreaterThanOrEqual(metrics.headerHeight - 1e-6);
          expect(shown.left + shown.width).toBeLessThanOrEqual(
            metrics.headerWidth + area.width + 1e-6,
          );
          expect(shown.top + shown.height).toBeLessThanOrEqual(
            metrics.headerHeight + area.height + 1e-6,
          );
          // It covers its cell.
          expect(shown.left).toBeLessThanOrEqual(cell.left + 1e-6);
          expect(shown.top).toBeLessThanOrEqual(cell.top + 1e-6);
          expect(shown.left + shown.width).toBeGreaterThanOrEqual(cell.left + cell.width - 1e-6);
          expect(shown.top + shown.height).toBeGreaterThanOrEqual(cell.top + cell.height - 1e-6);
          // Text that does not fit ends with the area.
          expect(shown.left + shown.maxWidth).toBeLessThanOrEqual(
            metrics.headerWidth + area.width + 1e-6,
          );
        }
      }
    }
  });

  test("that is shown leaves with its cell, and that is hidden stays in view", () => {
    for (const zoom of [0.5, 1]) {
      const view = viewAt({ zoom }, 100, 10);
      const { metrics, size } = view;
      const area = cellArea(metrics, size);
      const gone = boxIn(view, 20, 10);
      const shown = editorBox(metrics, size, gone, zoom, LEAST, 1, true);
      expect(shown.top + shown.height).toBeLessThanOrEqual(gone.top + gone.height + shown.height);
      expect(shown.top).toBeLessThan(0);
      const hidden = editorBox(metrics, size, gone, zoom, LEAST, 1, false);
      expect(hidden.top).toBe(metrics.headerHeight);
      const below = editorBox(metrics, size, boxIn(view, 900, 10), zoom, LEAST, 1, false);
      expect(below.top + below.height).toBe(metrics.headerHeight + area.height);
    }
  });
});

describe("what is in a cell", () => {
  test("has a filter button that grows with it and stays where it is in it", () => {
    for (const zoom of ZOOM_STEPS) {
      const width = 112 * zoom;
      const button = filterButtonBox(100, 50, width, 24 * zoom, zoom);
      expect(button?.size).toBeCloseTo(14 * zoom, 9);
      expect(100 + width - (button?.x ?? 0)).toBeCloseTo(17 * zoom, 9);
      const inside = (button?.y ?? 0) - 50;
      expect(inside).toBeGreaterThanOrEqual(0);
      expect(inside + 14 * zoom).toBeLessThanOrEqual(24 * zoom);
      expect(filterButtonBox(0, 0, 17 * zoom, 24 * zoom, zoom)).toBeNull();
    }
    expect(new FilterRowMap(10, []).visibleRows).toBe(10);
  });
});

describe("labels of headers", () => {
  test("are left out until the rest has room", () => {
    expect(labelStep(24, 13)).toBe(1);
    expect(labelStep(13, 13)).toBe(1);
    expect(labelStep(12, 13)).toBe(2);
    expect(labelStep(6, 13)).toBe(5);
    expect(labelStep(2, 13)).toBe(10);
    expect(labelStep(1, 13)).toBe(20);
    expect(labelStep(1, 113)).toBe(200);
    expect(labelStep(0, 13)).toBe(20);
    for (let size = 1; size < 40; size++) {
      for (let needed = 1; needed < 300; needed += 7) {
        const step = labelStep(size, needed);
        expect(step * size).toBeGreaterThanOrEqual(needed);
        expect(String(step)).toMatch(/^[125]0*$/);
      }
    }
  });
});
