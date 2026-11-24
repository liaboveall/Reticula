// Frames and text sizes made up for the tests of this directory.

import {
  FLAG_NUMBER,
  MAX_LEFT_OVERFLOW_LOOKBACK,
  PAGE_COLS,
  PAGE_ROWS,
} from "@reticula/protocol/viewport";
import { PageCache } from "../cache.ts";
import { type SheetLayout, SparseAxis, visibleCells } from "../layout.ts";
import type { Page } from "../page.ts";
import { readTheme, type Theme } from "../theme.ts";
import { Colors } from "./color.ts";
import type { Frame, GridCellStyle } from "./frame.ts";
import { type Scene, SceneBuilder } from "./scene.ts";
import type { TextMeasure } from "./text-measure.ts";

const sizeOf = (font: string) => Number.parseFloat(/([\d.]+)px/.exec(font)?.[1] ?? "0");

/** Advances that are nothing like whole pixels, and differ between characters. */
export const advanceOf = (font: string, character: string): number =>
  sizeOf(font) * (0.4 + (character.charCodeAt(0) % 7) * 0.037) + (font.includes("700") ? 0.31 : 0);

export const measure: TextMeasure = {
  width: (font, text) => [...text].reduce((sum, character) => sum + advanceOf(font, character), 0),
  line: (font) => ({ ascent: sizeOf(font) * 1.08, descent: sizeOf(font) * 0.25 }),
  ink: (font, text) => ({
    left: 0,
    right: measure.width(font, text),
    ascent: sizeOf(font) * 0.8,
    descent: sizeOf(font) * 0.2,
  }),
};

export const colors = new Colors((css) => css);

export const theme: Theme = (() => {
  const read = globalThis.getComputedStyle;
  Object.assign(globalThis, { getComputedStyle: () => ({ getPropertyValue: () => "" }) });
  const made = readTheme({} as HTMLElement);
  Object.assign(globalThis, { getComputedStyle: read });
  return made;
})();

export const PLAIN: GridCellStyle = {
  bold: false,
  italic: false,
  strikethrough: false,
  align: "auto",
  textColor: undefined,
  fillColor: undefined,
  borderTop: "none",
  borderRight: "none",
  borderBottom: "none",
  borderLeft: "none",
};

export interface TestCell {
  readonly text: string;
  readonly flags?: number;
  readonly style?: number;
}

/** A sheet's cells by `"row:col"`. */
export type TestCells = Readonly<Record<string, TestCell | string>>;

const cellOf = (cells: TestCells, row: number, col: number): TestCell => {
  const cell = cells[`${row}:${col}`] ?? "";
  return typeof cell === "string" ? { text: cell } : cell;
};

/** The page the engine would send of a sheet, with what overflows into it. */
export function pageOf(
  cells: TestCells,
  pageRow: number,
  pageCol: number,
  rowCount: number = PAGE_ROWS,
  colCount: number = PAGE_COLS,
): Page {
  const firstRow = pageRow * PAGE_ROWS;
  const firstCol = pageCol * PAGE_COLS;
  const count = rowCount * colCount;
  const texts: string[] = [];
  const styleIds = new Uint16Array(count);
  const flags = new Uint16Array(count);
  const overflows: Page["overflows"][number][] = [];
  for (let row = 0; row < rowCount; row++) {
    for (let col = 0; col < colCount; col++) {
      const cell = cellOf(cells, firstRow + row, firstCol + col);
      styleIds[texts.length] = cell.style ?? 0;
      flags[texts.length] = cell.flags ?? 0;
      texts.push(cell.text);
    }
    const first = cellOf(cells, firstRow + row, firstCol).text;
    const least = Math.max(0, firstCol - MAX_LEFT_OVERFLOW_LOOKBACK);
    for (let col = firstCol - 1; first === "" && col >= least; col--) {
      const cell = cellOf(cells, firstRow + row, col);
      if (cell.text !== "") {
        if (!((cell.flags ?? 0) & FLAG_NUMBER)) {
          const { text, style = 0 } = cell;
          overflows.push({ rowOffset: row, originCol: col, styleId: style, text });
        }
        break;
      }
    }
  }
  return {
    sheet: 0,
    epoch: 1,
    firstRow,
    firstCol,
    rowCount,
    colCount,
    rowIds: new BigUint64Array(rowCount),
    colIds: new BigUint64Array(colCount),
    texts,
    styleIds,
    flags,
    overflows,
    byteLength: 0,
  };
}

/** A cache with the pages of some page rows and columns, but for the ones left out. */
export function cacheOf(
  cells: TestCells,
  pageRows: number,
  pageCols: number,
  without: readonly string[] = [],
): PageCache {
  const cache = new PageCache();
  cache.retain({ firstRow: 0, endRow: pageRows, firstCol: 0, endCol: pageCols });
  for (let row = 0; row < pageRows; row++) {
    for (let col = 0; col < pageCols; col++) {
      if (!without.includes(`${row}:${col}`)) {
        cache.accept(pageOf(cells, row, col));
      }
    }
  }
  return cache;
}

export const METRICS = {
  rowHeight: 20,
  colWidth: 75,
  headerHeight: 30,
  headerWidth: 45,
  scrollbarSize: 10,
};

export function axes(
  rows: readonly (readonly [number, number])[] = [],
  cols: readonly (readonly [number, number])[] = [],
  frozenRows = 0,
  frozenCols = 0,
): SheetLayout {
  return {
    rowAxis: new SparseAxis(METRICS.rowHeight, rows),
    colAxis: new SparseAxis(METRICS.colWidth, cols),
    frozenRows,
    frozenCols,
  };
}

/** A frame of a sheet of 40 by 40 cells that has not arrived, unless told otherwise. */
export function frameOf(changes: Partial<Frame> = {}): Frame {
  const frame = {
    size: { width: 350, height: 190 },
    scale: 1,
    metrics: METRICS,
    theme,
    scroll: { top: 0, left: 0 },
    sheet: { rows: 40, cols: 40 },
    layout: axes(),
    cache: new PageCache(),
    selection: null,
    ...changes,
  };
  const { metrics, sheet, scroll, size, layout } = frame;
  return { visible: visibleCells(metrics, sheet, scroll, size, layout), ...frame };
}

export function sceneOf(frame: Frame, builder = new SceneBuilder(measure, colors)): Scene {
  return builder.build(frame);
}

export interface PaintedRect {
  readonly left: number;
  readonly top: number;
  readonly width: number;
  readonly height: number;
  readonly color: number;
}

/** All the rectangles of a scene in the order they are painted, where they are on the screen. */
export function rectsOf(scene: Scene): PaintedRect[] {
  const painted: PaintedRect[] = [];
  const add = (
    rects: Scene["rects"],
    first: number,
    end: number,
    x: number,
    y: number,
    clip?: { left: number; top: number; right: number; bottom: number },
  ) => {
    for (let index = first; index < end; index++) {
      const at = index * 4;
      const left = Math.max(clip?.left ?? 0, x + (rects.geometry[at] ?? 0));
      const top = Math.max(clip?.top ?? 0, y + (rects.geometry[at + 1] ?? 0));
      const right = x + (rects.geometry[at] ?? 0) + (rects.geometry[at + 2] ?? 0);
      const bottom = y + (rects.geometry[at + 1] ?? 0) + (rects.geometry[at + 3] ?? 0);
      const width = Math.min(clip?.right ?? right, right) - left;
      const height = Math.min(clip?.bottom ?? bottom, bottom) - top;
      if (width > 0 && height > 0) {
        painted.push({ left, top, width, height, color: rects.colors[index] ?? 0 });
      }
    }
  };
  for (const step of scene.steps) {
    if (step.kind === "rects") {
      add(scene.rects, step.first, step.end, 0, 0);
    } else if (step.kind === "fills" || step.kind === "borders") {
      const rects = step.layout[step.kind];
      add(rects, 0, rects.length, step.x, step.y, step.clip);
    }
  }
  return painted;
}
