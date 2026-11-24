import type { Frame, Page } from "@reticula/grid/internals";
import {
  FLAG_BOOL,
  FLAG_ERROR,
  FLAG_NUMBER,
  PAGE_COLS,
  PAGE_ROWS,
} from "@reticula/protocol/viewport";

export type Style = NonNullable<Frame["styles"]>[number];

const PLAIN: Style = {
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

interface Cell {
  readonly text: string;
  readonly flags: number;
  readonly styleId: number;
}

/** A sheet held by the page itself: what the engine would send of it, as pages. */
export class SyntheticSheet {
  readonly rows: number;
  readonly cols: number;
  readonly styles: Style[] = [PLAIN];
  #cells = new Map<string, Cell>();
  #filler: ((row: number, col: number) => string) | null = null;

  constructor(rows = 1000, cols = 200) {
    this.rows = rows;
    this.cols = cols;
  }

  /** Text, with the kind it reads as: a number, a truth value, an error, or just text. */
  put(row: number, col: number, text: string, style: Partial<Style> = {}): this {
    this.#cells.set(`${row}:${col}`, { text, flags: flagsOf(text), styleId: this.#styleId(style) });
    return this;
  }

  /** What is in every cell nothing was put into. */
  fill(filler: (row: number, col: number) => string): this {
    this.#filler = filler;
    return this;
  }

  cell(row: number, col: number): Cell {
    const cell = this.#cells.get(`${row}:${col}`);
    if (cell) {
      return cell;
    }
    const text = this.#filler?.(row, col) ?? "";
    return { text, flags: flagsOf(text), styleId: 0 };
  }

  page(pageRow: number, pageCol: number): Page {
    const firstRow = pageRow * PAGE_ROWS;
    const firstCol = pageCol * PAGE_COLS;
    const rowCount = Math.min(PAGE_ROWS, this.rows - firstRow);
    const colCount = Math.min(PAGE_COLS, this.cols - firstCol);
    const texts: string[] = [];
    const styleIds = new Uint16Array(rowCount * colCount);
    const flags = new Uint16Array(rowCount * colCount);
    const overflows: Page["overflows"][number][] = [];
    for (let row = 0; row < rowCount; row++) {
      for (let col = 0; col < colCount; col++) {
        const cell = this.cell(firstRow + row, firstCol + col);
        styleIds[texts.length] = cell.styleId;
        flags[texts.length] = cell.flags;
        texts.push(cell.text);
      }
      // The nearest text to the left is the only one that can reach this page.
      for (let originCol = firstCol - 1; originCol >= 0; originCol--) {
        const cell = this.cell(firstRow + row, originCol);
        if (cell.text) {
          const align = this.styles[cell.styleId]?.align ?? "auto";
          if (cell.flags === 0 && (align === "auto" || align === "left")) {
            overflows.push({ rowOffset: row, originCol, styleId: cell.styleId, text: cell.text });
          }
          break;
        }
      }
    }
    const ids = (first: number, count: number) =>
      BigUint64Array.from({ length: count }, (_, index) => BigInt(first + index + 1));
    return {
      sheet: 0,
      epoch: 0,
      firstRow,
      firstCol,
      rowCount,
      colCount,
      rowIds: ids(firstRow, rowCount),
      colIds: ids(firstCol, colCount),
      texts,
      styleIds,
      flags,
      overflows,
      byteLength: 0,
    };
  }

  #styleId(style: Partial<Style>): number {
    if (Object.keys(style).length === 0) {
      return 0;
    }
    const whole = { ...PLAIN, ...style };
    const key = JSON.stringify(whole);
    const known = this.styles.findIndex((other) => JSON.stringify(other) === key);
    return known >= 0 ? known : this.styles.push(whole) - 1;
  }
}

function flagsOf(text: string): number {
  if (/^-?[\d,]+(\.\d+)?%?$/.test(text)) {
    return FLAG_NUMBER;
  }
  if (text === "TRUE" || text === "FALSE") {
    return FLAG_BOOL;
  }
  return text.startsWith("#") ? FLAG_ERROR : 0;
}
