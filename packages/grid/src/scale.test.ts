import { describe, expect, test } from "vitest";
import { PageCache } from "./cache.ts";
import {
  cellAt,
  cellBox,
  cellsOfPages,
  clampScroll,
  cornerOf,
  DEFAULT_METRICS,
  frozenPixels,
  headerHitAt,
  pagesToHold,
  reanchor,
  SparseAxis,
  scrollAtThumb,
  scrollLimits,
  scrollToCorner,
  scrollToReveal,
  thumb,
  type View,
  viewMetrics,
  visibleCells,
} from "./layout.ts";
import { colors, frameOf, pageOf, rectsOf, sceneOf, theme } from "./render/fixtures.ts";
import { ZOOM_STEPS } from "./zoom.ts";

// Far down a sheet, offsets are numbers of ten digits with fractions: what is
// computed from them has to come out to the row, at any zoom and density.

const DENSITIES = [0.8, 1, 1.1, 1.25, 1.5, 1.75, 2, 2.625, 3, 4];
const COUNTS = [10_000_000, 99_999_999, 100_000_000];
const COLS = 200;

function viewOf(rows: number, zoom: number, scale: number, frozenCols = 0): View {
  const size = { width: Math.round(1180 * scale) / scale, height: Math.round(640 * scale) / scale };
  return {
    metrics: viewMetrics(DEFAULT_METRICS, zoom, scale),
    sheet: { rows, cols: COLS },
    size,
    layout: {
      rowAxis: new SparseAxis(DEFAULT_METRICS.rowHeight, [], scale, zoom),
      colAxis: new SparseAxis(DEFAULT_METRICS.colWidth, [[3, 41]], scale, zoom),
      frozenRows: 0,
      frozenCols,
    },
    scroll: { top: 0, left: 0 },
  };
}

/** A view scrolled as far as the grid would let it, on whole device pixels. */
function scrolled(view: View, scale: number, scroll: View["scroll"]): View {
  const limits = scrollLimits(view.metrics, view.sheet, view.size, view.layout);
  const { top, left } = clampScroll(scroll, limits);
  const snap = (css: number) => Math.round(css * scale) / scale;
  return { ...view, scroll: { top: snap(top), left: snap(left) } };
}

const visibleIn = ({ metrics, sheet, scroll, size, layout }: View) =>
  visibleCells(metrics, sheet, scroll, size, layout);
const boxIn = ({ metrics, sheet, scroll, size, layout }: View, row: number, col: number) =>
  cellBox(metrics, scroll, row, col, size, sheet, layout);
const cellIn = ({ metrics, sheet, scroll, size, layout }: View, x: number, y: number) =>
  cellAt(metrics, sheet, scroll, size, x, y, layout);
const headerIn = ({ metrics, sheet, scroll, size, layout }: View, x: number, y: number) =>
  headerHitAt(metrics, sheet, scroll, size, x, y, layout);
const revealed = (view: View, scale: number, row: number) =>
  scrolled(
    view,
    scale,
    scrollToReveal(view.metrics, view.size, view.scroll, row, 0, view.sheet, view.layout),
  );

/** Every number of rows, zoom and density there is, with a name for what fails. */
function* settings(): Generator<[rows: number, zoom: number, scale: number, name: string]> {
  for (const rows of COUNTS) {
    for (const zoom of ZOOM_STEPS) {
      for (const scale of DENSITIES) {
        yield [rows, zoom, scale, `${rows} rows, zoom ${zoom}, scale ${scale}`];
      }
    }
  }
}

describe("at the end of many rows", () => {
  test("the last row is in view, whole, and right above the scrollbar", () => {
    for (const [rows, zoom, scale, name] of settings()) {
      const view = scrolled(viewOf(rows, zoom, scale), scale, { top: Infinity, left: 0 });
      const { firstRow, endRow } = visibleIn(view);
      expect(endRow, name).toBe(rows);
      const { rowHeight, scrollbarSize } = view.metrics;
      expect(endRow - firstRow, name).toBeLessThan(640 / rowHeight + 2);
      const last = boxIn(view, rows - 1, 0);
      const bottom = view.size.height - scrollbarSize;
      expect(Math.abs(last.top + last.height - bottom) * scale, name).toBeLessThan(1e-3);
    }
  });

  test("every pixel is the row's that is drawn there", () => {
    for (const [rows, zoom, scale, name] of settings()) {
      const view = scrolled(viewOf(rows, zoom, scale, 2), scale, { top: Infinity, left: 77 });
      const { headerWidth } = view.metrics;
      for (const row of [rows - 1, rows - 2, rows - 5]) {
        const { top, height } = boxIn(view, row, 0);
        const pixel = 1 / scale;
        for (const y of [top, top + pixel / 2, top + height / 2, top + height - pixel / 2]) {
          const cell = cellIn(view, headerWidth + 3, y);
          expect(cell?.row, name).toBe(row);
          expect(cell?.col, name).toBe(0);
        }
        expect(cellIn(view, headerWidth + 3, top - pixel / 2)?.row, name).toBe(row - 1);
        const header = headerIn(view, 4, top + height / 2);
        expect(header, name).toEqual({ kind: "row", row });
      }
      const { top, height } = boxIn(view, rows - 1, 0);
      // The end of the last one is the end of the view.
      expect(cellIn(view, headerWidth + 3, top + height + 1e-6), name).toBeNull();
    }
  });

  test("a row is revealed by as little as it takes, from either side", () => {
    for (const [rows, zoom, scale, name] of settings()) {
      const start = viewOf(rows, zoom, scale);
      const limits = scrollLimits(start.metrics, start.sheet, start.size, start.layout);
      const last = revealed(start, scale, rows - 1);
      expect(Math.abs(last.scroll.top - limits.top) * scale, name).toBeLessThan(1e-3);
      for (const row of [rows - 1000, Math.floor(rows / 2), 7_654_321]) {
        for (const from of [start, last]) {
          const view = revealed(from, scale, row);
          const { top, height } = boxIn(view, row, 0);
          const { headerHeight, scrollbarSize } = view.metrics;
          const first = Math.abs(top - headerHeight) * scale;
          const end = Math.abs(top + height - (view.size.height - scrollbarSize)) * scale;
          expect(Math.min(first, end), name).toBeLessThan(1e-3);
        }
      }
    }
  });

  test("where a view is can be told, and gone back to", () => {
    for (const [rows, zoom, scale, name] of settings()) {
      for (const row of [rows - 100, 9_999_999 - 64, 5_000_001]) {
        const there = scrolled(
          viewOf(rows, zoom, scale),
          scale,
          scrollToCorner(viewOf(rows, zoom, scale), {
            row: { index: row, fraction: 0.5 },
            col: { index: 0, fraction: 0 },
          }),
        );
        const corner = cornerOf(there);
        expect(corner.row.index, name).toBeLessThanOrEqual(row);
        const back = scrolled(there, scale, scrollToCorner(there, corner));
        expect(Math.abs(back.scroll.top - there.scroll.top) * scale, name).toBeLessThan(1e-3);
        // Zooming at a point leaves the row that is there under it.
        const point = { x: 300, y: 200 };
        const under = cellIn(there, point.x, point.y);
        const closer = { ...viewOf(rows, Math.min(2, zoom * 1.25), scale), scroll: there.scroll };
        const zoomed = scrolled(closer, scale, reanchor(there, closer, point));
        expect(
          Math.abs((cellIn(zoomed, point.x, point.y)?.row ?? 0) - (under?.row ?? 0)),
          name,
        ).toBeLessThanOrEqual(1);
      }
    }
  });
});

describe("the scrollbar of many rows", () => {
  test("has its thumb at either end there, and goes to any place between", () => {
    for (const rows of COUNTS) {
      const content = rows * DEFAULT_METRICS.rowHeight;
      const track = 600;
      const room = 604;
      const at = (scroll: number) => thumb(track, content, room, scroll, 32);
      expect(at(0)).toEqual({ offset: 0, length: 32 });
      expect(at(content - room)).toEqual({ offset: track - 32, length: 32 });
      for (const part of [0.1, 0.5, 0.999]) {
        const scroll = scrollAtThumb(track, content, room, (track - 32) * part, 32);
        expect(scroll / (content - room)).toBeCloseTo(part, 9);
        expect(at(scroll)?.offset).toBeCloseTo((track - 32) * part, 6);
      }
      expect(scrollAtThumb(track, content, room, track - 32, 32)).toBe(content - room);
    }
  });
});

describe("the pages of many rows", () => {
  test("are held to the last one, and are told apart", () => {
    for (const rows of COUNTS) {
      const visible = { firstRow: rows - 27, endRow: rows, firstCol: 190, endCol: 200 };
      const held = pagesToHold(visible, { rows, cols: COLS }, { rows: 0.5, cols: 1 });
      expect(held.endRow).toBe(Math.ceil(rows / 64));
      expect(cellsOfPages(held, { rows, cols: COLS }).endRow).toBe(rows);
      const cache = new PageCache();
      cache.retain(held);
      const at = (pageRow: number, pageCol: number) => ({
        ...pageOf({}, 0, 0),
        firstRow: pageRow * 64,
        firstCol: pageCol * 16,
      });
      const last = at(held.endRow - 1, 12);
      const before = at(held.endRow - 1, 11);
      expect(cache.accept(last)).toBe(true);
      expect(cache.accept(before)).toBe(true);
      expect(cache.pageOf(rows - 1, 199)).toBe(last);
      expect(cache.pageOf(rows - 1, 191)).toBe(before);
      expect(cache.accept(at(held.endRow, 12))).toBe(false);
    }
  });
});

test("the last of many rows are drawn where they are, with their numbers", () => {
  const rows = 100_000_000;
  for (const [zoom, scale] of [
    [1, 1],
    [0.5, 1.25],
    [1.5, 2],
  ] as const) {
    const view = scrolled(viewOf(rows, zoom, scale), scale, { top: Infinity, left: 0 });
    const scene = sceneOf(
      frameOf({
        ...view,
        zoom,
        scale,
        selection: { row: rows - 1, col: 0 },
        selectionRange: { firstRow: 0, endRow: rows, firstCol: 0, endCol: 1 },
      }),
    );
    const labels = scene.steps.flatMap((step) =>
      step.kind === "labels" ? step.runs.texts.slice(0, step.runs.length) : [],
    );
    expect(labels).toContain(String(rows));
    expect(labels).not.toContain(String(rows + 1));
    const bottom = Math.round((view.size.height - view.metrics.scrollbarSize) * scale);
    const outline = rectsOf(scene).filter((rect) => rect.color === colors.parse(theme.selection));
    expect(Math.max(...outline.map((rect) => rect.top + rect.height))).toBe(bottom);
    expect(frozenPixels(view.metrics, view.sheet, view.size, view.layout).frozenCols).toBe(0);
    for (const rect of rectsOf(scene)) {
      expect(rect.top + rect.height).toBeLessThanOrEqual(scene.height);
    }
  }
});
