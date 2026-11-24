import {
  CELL_LEN,
  CELL_TEXT_LEN_AT,
  CELL_TEXT_START_AT,
  COL_COUNT_AT,
  EPOCH_AT,
  FIRST_COL_AT,
  FIRST_ROW_AT,
  HEADER_LEN,
  ID_LEN,
  PAGE_COLS,
  PAGE_ROWS,
  ROW_COUNT_AT,
  TEXT_LEN_AT,
  VERSION,
  VERSION_AT,
} from "@reticula/protocol/viewport";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { Grid, type GridOptions, type SheetConfig, type Subscription } from "./grid.ts";
import { DEFAULT_METRICS } from "./layout.ts";

// The grid in a document that is none: elements remember what is set on them,
// and nothing is drawn.

class FakeElement extends EventTarget {
  readonly children: FakeElement[] = [];
  readonly attributes = new Map<string, string>();
  readonly style = { setProperty: () => {} };
  readonly dataset = {};
  readonly classList = { add: () => {}, remove: () => {} };
  tabIndex = -1;
  textContent = "";
  hidden = false;
  id = "";
  headers = "";
  readonly clientWidth = 64 + 800 + 12;
  readonly clientHeight = 24 + 480 + 12;

  setAttribute(name: string, value: string): void {
    this.attributes.set(name, value);
  }

  removeAttribute(name: string): void {
    this.attributes.delete(name);
  }

  append(...children: FakeElement[]): void {
    this.children.push(...children);
  }

  insertRow(): FakeElement {
    const row = new FakeElement();
    this.append(row);
    return row;
  }

  createTHead = () => this.insertRow();
  createTBody = () => this.insertRow();
  before(): void {}
  remove(): void {}
  focus(): void {}

  getBoundingClientRect() {
    return { left: 0, top: 0 };
  }

  getContext() {
    const measured = {
      width: 7,
      fontBoundingBoxAscent: 10,
      fontBoundingBoxDescent: 3,
    };
    return new Proxy<Record<string, unknown>>(
      {
        fillStyle: "#000000",
        measureText: (text: string) => ({ ...measured, width: 7 * text.length }),
      },
      { get: (state, name: string) => (name in state ? state[name] : () => {}) },
    );
  }

  /** The text of every cell that is shown, if this is a table. */
  get cells(): string[] {
    return this.children.flatMap((child) =>
      child.hidden ? [] : child.children.length > 0 ? child.cells : [child.textContent],
    );
  }
}

let frames: (() => void)[] = [];
let languageChanged = () => {};

beforeEach(() => {
  frames = [];
  const observer = (onMade: (callback: () => void) => void) =>
    class {
      constructor(callback: () => void) {
        onMade(callback);
      }
      observe(): void {}
      disconnect(): void {}
    };
  vi.stubGlobal("document", {
    documentElement: { lang: "en" },
    createElement: () => new FakeElement(),
  });
  vi.stubGlobal("window", {
    devicePixelRatio: 1,
    clearTimeout: () => {},
    removeEventListener: () => {},
  });
  vi.stubGlobal("navigator", {});
  vi.stubGlobal("matchMedia", () => Object.assign(new EventTarget(), { matches: false }));
  vi.stubGlobal("getComputedStyle", () => ({ getPropertyValue: () => "" }));
  vi.stubGlobal(
    "ResizeObserver",
    observer(() => {}),
  );
  vi.stubGlobal(
    "MutationObserver",
    observer((callback) => {
      languageChanged = callback;
    }),
  );
  vi.stubGlobal("requestAnimationFrame", (callback: () => void) => frames.push(callback));
  vi.stubGlobal("cancelAnimationFrame", () => {});
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

const rowId = (row: number) => 1000n + BigInt(row);
const colId = (col: number) => 500n + BigInt(col);

/** A page the way the engine sends it. Moved, its rows or columns have the identifiers of others. */
function pageOf(
  pageRow: number,
  pageCol: number,
  textAt: (row: number, col: number) => string,
  ids = { row: rowId, col: colId },
): ArrayBuffer {
  const cells = PAGE_ROWS * PAGE_COLS;
  const texts = Array.from({ length: cells }, (_, cell) =>
    textAt(
      pageRow * PAGE_ROWS + Math.floor(cell / PAGE_COLS),
      pageCol * PAGE_COLS + (cell % PAGE_COLS),
    ),
  );
  const pool = new TextEncoder().encode(texts.join(""));
  const cellsAt = HEADER_LEN + (PAGE_ROWS + PAGE_COLS) * ID_LEN;
  const buffer = new ArrayBuffer(cellsAt + cells * CELL_LEN + pool.length);
  const view = new DataView(buffer);
  view.setUint32(VERSION_AT, VERSION, true);
  view.setUint32(FIRST_ROW_AT, pageRow * PAGE_ROWS, true);
  view.setUint32(FIRST_COL_AT, pageCol * PAGE_COLS, true);
  view.setUint16(ROW_COUNT_AT, PAGE_ROWS, true);
  view.setUint16(COL_COUNT_AT, PAGE_COLS, true);
  view.setUint32(TEXT_LEN_AT, pool.length, true);
  view.setUint32(EPOCH_AT, 1, true);
  for (let row = 0; row < PAGE_ROWS; row++) {
    view.setBigUint64(HEADER_LEN + row * ID_LEN, ids.row(pageRow * PAGE_ROWS + row), true);
  }
  for (let col = 0; col < PAGE_COLS; col++) {
    const at = HEADER_LEN + (PAGE_ROWS + col) * ID_LEN;
    view.setBigUint64(at, ids.col(pageCol * PAGE_COLS + col), true);
  }
  let start = 0;
  for (const [cell, text] of texts.entries()) {
    view.setUint32(cellsAt + cell * CELL_LEN + CELL_TEXT_START_AT, start, true);
    view.setUint16(cellsAt + cell * CELL_LEN + CELL_TEXT_LEN_AT, text.length, true);
    start += text.length;
  }
  new Uint8Array(buffer).set(pool, cellsAt + cells * CELL_LEN);
  return buffer;
}

const textOf = (row: number, col: number) =>
  col === 2 ? "a long text of twenty-nine letters" : row === 1 ? "x" : "";

function open(options: Partial<GridOptions> = {}, sheet: Partial<SheetConfig> = {}) {
  const host = new FakeElement();
  let held: Subscription | null = null;
  const grid = new Grid(host as unknown as HTMLElement, {
    subscribe: (subscription) => {
      held = subscription;
    },
    ...options,
  });
  const send = (ids = { row: rowId, col: colId }) => {
    const { firstRow = 0, rowCount = 0, firstCol = 0, colCount = 0 } = held ?? {};
    for (let row = firstRow / PAGE_ROWS; row * PAGE_ROWS < firstRow + rowCount; row++) {
      for (let col = firstCol / PAGE_COLS; col * PAGE_COLS < firstCol + colCount; col++) {
        grid.acceptPage(pageOf(row, col, textOf, ids));
      }
    }
  };
  const arrive = (row: number, col: number, ids = { row: rowId, col: colId }) =>
    grid.acceptPage(pageOf(row, col, textOf, ids));
  const draw = () => {
    for (const frame of frames.splice(0)) {
      frame();
    }
  };
  grid.setSheet({ rows: 10_000, cols: 64, ...sheet });
  draw();
  send();
  draw();
  const [, , , , , mirror] = host.children;
  if (!mirror) {
    throw new Error("the grid has no mirror");
  }
  return { grid, host, mirror, send, arrive, draw };
}

const cells = (firstRow: number, endRow: number, firstCol: number, endCol: number) => ({
  firstRow,
  endRow,
  firstCol,
  endCol,
});

function press(host: FakeElement, key: string, keys: Partial<KeyboardEvent> = {}): boolean {
  const event = Object.assign(new Event("keydown", { cancelable: true }), {
    key,
    shiftKey: false,
    ctrlKey: false,
    metaKey: false,
    altKey: false,
    isComposing: false,
    ...keys,
  });
  host.dispatchEvent(event);
  return event.defaultPrevented;
}

describe("whole rows and columns", () => {
  test("are selected, with the active cell staying where it is along them", () => {
    const { grid } = open();
    grid.select({ row: 7, col: 3 });
    grid.selectRows(5, 7);
    expect(grid.selectionScope).toBe("rows");
    expect(grid.selectionRange).toEqual(cells(5, 8, 0, 64));
    expect(grid.selection).toEqual({ row: 7, col: 3 });
    grid.selectCols(3, 9);
    expect(grid.selectionScope).toBe("cols");
    expect(grid.selectionRange).toEqual(cells(0, 10_000, 3, 10));
    expect(grid.selection).toEqual({ row: 7, col: 3 });
    grid.selectRows(20, 20);
    expect(grid.selection).toEqual({ row: 20, col: 0 });
    expect(grid.selectionRanges).toEqual([cells(20, 21, 0, 64)]);
  });

  test("are scrolled to, but not along them", () => {
    const { grid, draw } = open();
    grid.scrollTo({ left: 500 });
    grid.selectRows(200, 201);
    draw();
    expect(grid.stats().scroll).toMatchObject({ left: 500 });
    expect(grid.stats().visible.endRow).toBeGreaterThan(200);
    grid.extendSelection(1, 0);
    expect(grid.selectionRange).toEqual(cells(200, 203, 0, 64));
    expect(grid.stats().scroll.left).toBe(500);
    const { top } = grid.stats().scroll;
    grid.selectCols(40, 40);
    draw();
    expect(grid.stats().scroll.top).toBe(top);
    expect(grid.stats().scroll.left).toBeGreaterThan(500);
  });
});

describe("several ranges", () => {
  test("are selected one after the other, and the last one has the active cell", () => {
    const { grid } = open();
    grid.select({ row: 1, col: 1 });
    grid.addSelectionRange(cells(5, 8, 2, 4));
    expect(grid.selectionRanges).toEqual([cells(1, 2, 1, 2), cells(5, 8, 2, 4)]);
    expect(grid.selection).toEqual({ row: 5, col: 2 });
    grid.addSelectionRange(cells(9, 10, -3, 200));
    expect(grid.selectionScope).toBe("rows");
    expect(grid.selectionRanges).toHaveLength(3);
    expect(grid.selectionRange).toEqual(cells(9, 10, 0, 64));
    grid.addSelectionRange(cells(0, 10_000, 3, 4));
    expect(grid.selectionScope).toBe("cols");
    // Nothing of it is on the sheet.
    grid.addSelectionRange(cells(0, 1, 64, 70));
    expect(grid.selectionRanges).toHaveLength(4);
  });
});

describe("rows and columns", () => {
  test("are moved past their neighbours, and the selection with them", () => {
    const onMoveRows = vi.fn();
    const onMoveCols = vi.fn();
    const { grid, send } = open({ onMoveRows, onMoveCols });
    grid.selectRange({ row: 4, col: 1 }, { row: 5, col: 2 });
    expect(grid.moveSelectedRows(1)).toBe(true);
    expect(onMoveRows).toHaveBeenLastCalledWith([{ firstId: "1004", len: 2 }], "1006");
    expect(grid.selectionRange).toEqual(cells(5, 7, 1, 3));

    // What is known of the rows is from before: the next key would move another one.
    expect(grid.moveSelectedRows(1)).toBe(false);
    const moved = (row: number) =>
      row === 4 ? rowId(6) : row < 7 && row > 4 ? rowId(row - 1) : rowId(row);
    send({ row: moved, col: colId });
    expect(grid.moveSelectedRows(-1)).toBe(true);
    expect(onMoveRows).toHaveBeenLastCalledWith([{ firstId: "1004", len: 2 }], "1003");
    expect(grid.selectionRange).toEqual(cells(4, 6, 1, 3));
    send();

    expect(grid.moveSelectedCols(-1)).toBe(true);
    expect(onMoveCols).toHaveBeenLastCalledWith([{ firstId: "501", len: 2 }], null);
    expect(grid.selectionRange).toEqual(cells(4, 6, 0, 2));
    send();
    expect(grid.moveSelectedCols(-1)).toBe(false);
    expect(onMoveCols).toHaveBeenCalledTimes(1);
  });

  test("are moved another time only once all that is known of them is from after the first", () => {
    const onMoveRows = vi.fn();
    const { grid, arrive, send } = open({ onMoveRows });
    // In the second page across, of which the rows are known from the first.
    grid.select({ row: 4, col: PAGE_COLS + 1 });
    expect(grid.moveSelectedRows(1)).toBe(true);
    const moved = (row: number) => (row === 4 ? rowId(5) : row === 5 ? rowId(4) : rowId(row));
    // A page that was on its way, and one that came before the one that counts.
    arrive(0, 1);
    expect(grid.moveSelectedRows(1)).toBe(false);
    arrive(0, 1, { row: moved, col: colId });
    expect(grid.moveSelectedRows(1)).toBe(false);
    expect(onMoveRows).toHaveBeenCalledTimes(1);
    send({ row: moved, col: colId });
    expect(grid.moveSelectedRows(1)).toBe(true);
    expect(onMoveRows).toHaveBeenLastCalledWith([{ firstId: "1004", len: 1 }], "1006");
  });

  test("are moved another time even if nothing came of the first", () => {
    vi.useFakeTimers({ toFake: ["performance"] });
    const onMoveRows = vi.fn();
    const { grid } = open({ onMoveRows });
    grid.select({ row: 4, col: 1 });
    expect(grid.moveSelectedRows(1)).toBe(true);
    vi.advanceTimersByTime(1_999);
    expect(grid.moveSelectedRows(1)).toBe(false);
    vi.advanceTimersByTime(2);
    expect(grid.moveSelectedRows(1)).toBe(true);
  });

  test("stay where they are at the edge, unknown, in several ranges, and without anybody to tell", () => {
    const onMoveRows = vi.fn();
    const { grid } = open({ onMoveRows });
    grid.select({ row: 0, col: 0 });
    expect(grid.moveSelectedRows(-1)).toBe(false);
    grid.select({ row: 9_999, col: 0 });
    expect(grid.moveSelectedRows(1)).toBe(false);
    // Far out of view.
    grid.select({ row: 5_000, col: 0 });
    expect(grid.moveSelectedRows(1)).toBe(false);
    grid.selectCols(1, 1);
    expect(grid.moveSelectedRows(1)).toBe(false);
    grid.select({ row: 2, col: 2 });
    expect(grid.moveSelectedCols(1)).toBe(false);
    grid.addSelectionRange(cells(5, 6, 0, 1));
    expect(grid.moveSelectedRows(1)).toBe(false);
    expect(onMoveRows).not.toHaveBeenCalled();
  });

  test("get a size, in bounds, or the standard one", () => {
    const onResizeRows = vi.fn();
    const onResizeCols = vi.fn();
    const { grid } = open({ onResizeRows, onResizeCols });
    const { rowHeight, colWidth } = DEFAULT_METRICS;
    grid.selectRows(2, 3);
    expect(grid.sizeOfSelection()).toEqual({ rowPx: rowHeight, colPx: colWidth });
    expect(grid.resizeSelectedRows(40.4)).toBe(true);
    expect(onResizeRows).toHaveBeenLastCalledWith([
      { run: { firstId: "1002", len: 2 }, sizePx: 40 },
    ]);
    expect(grid.sizeOfSelection().rowPx).toBe(40);
    // Shown without waiting for the engine.
    expect(grid.boxOf(3, 0).height).toBe(40);
    expect(grid.boxOf(4, 0).top - grid.boxOf(2, 0).top).toBe(80);
    grid.selectRows(3, 4);
    expect(grid.sizeOfSelection().rowPx).toBeNull();
    grid.resizeSelectedRows(null);
    expect(onResizeRows).toHaveBeenLastCalledWith([
      { run: { firstId: "1003", len: 2 }, sizePx: null },
    ]);
    expect(grid.sizeOfSelection().rowPx).toBe(rowHeight);
    expect(grid.boxOf(2, 0).height).toBe(40);

    grid.select({ row: 0, col: 1 });
    grid.addSelectionRange(cells(0, 1, 4, 6));
    grid.addSelectionRange(cells(3, 4, 5, 7));
    grid.resizeSelectedCols(1);
    expect(onResizeCols).toHaveBeenLastCalledWith([
      { run: { firstId: "501", len: 1 }, sizePx: 24 },
      { run: { firstId: "504", len: 3 }, sizePx: 24 },
    ]);
    grid.resizeSelectedCols(colWidth);
    expect(onResizeCols.mock.lastCall?.[0]).toMatchObject([{ sizePx: null }, { sizePx: null }]);
    grid.selectCols(0, 0);
    expect(grid.resizeSelectedRows(30)).toBe(false);
    expect(onResizeRows).toHaveBeenCalledTimes(2);
  });

  test("are as wide as their texts need, all at once", () => {
    const onResizeCols = vi.fn();
    const { grid } = open({ onResizeCols });
    grid.selectCols(1, 2);
    expect(grid.autofitSelectedCols()).toBe(true);
    const [sizes] = onResizeCols.mock.lastCall ?? [];
    expect(sizes).toMatchObject([{ run: { firstId: "501" } }, { run: { firstId: "502" } }]);
    expect(grid.boxOf(0, 1).width).toBe(sizes[0].sizePx);
    expect(sizes[0].sizePx).toBeLessThan(DEFAULT_METRICS.colWidth);
    expect(sizes[1].sizePx).toBeGreaterThan(7 * textOf(0, 2).length);
    // They are that wide now.
    expect(grid.autofitSelectedCols()).toBe(true);
    expect(onResizeCols).toHaveBeenCalledTimes(1);
    // Nothing is known of what is in these.
    grid.select({ row: 0, col: 60 });
    expect(grid.autofitSelectedCols()).toBe(false);
  });
});

test("a menu finds the selection, as much of it as is in view", () => {
  const { grid, draw } = open();
  grid.selectRange({ row: 2, col: 1 }, { row: 3, col: 2 });
  expect(grid.boxOfSelection()).toEqual({
    left: 64 + 112,
    top: 24 + 48,
    width: 224,
    height: 48,
  });
  grid.scrollTo({ top: 24 * 100 });
  draw();
  expect(grid.boxOfSelection()).toBeNull();
  grid.selectAll();
  expect(grid.boxOfSelection()).toEqual({ left: 64, top: 24, width: 800, height: 480 });
});

describe("the keyboard", () => {
  test("moves the selection of a grid without an editor, which has a name", () => {
    const { grid, host } = open();
    expect(host.tabIndex).toBe(0);
    expect(host.attributes.get("role")).toBe("application");
    grid.setStrings({ grid: "Cells", cellEditor: "Editor", selectedCell: "Selected" });
    expect(host.attributes.get("aria-label")).toBe("Cells");
    expect(press(host, "ArrowDown")).toBe(true);
    expect(press(host, "End", { ctrlKey: true })).toBe(true);
    expect(grid.selection).toEqual({ row: 9_999, col: 63 });
    expect(press(host, "Tab")).toBe(false);
    expect(press(host, "ArrowUp", { isComposing: true })).toBe(false);
    expect(grid.selection).toEqual({ row: 9_999, col: 63 });
  });

  test("is an editor's while there is one, and the grid's again after it", () => {
    const { grid, host } = open();
    const onStrings = vi.fn();
    const detach = grid.attachEditor(onStrings);
    expect(host.tabIndex).toBe(-1);
    expect(host.attributes.get("role")).toBe("group");
    grid.setStrings({ grid: "a", cellEditor: "b", selectedCell: "c" });
    expect(onStrings).toHaveBeenCalledTimes(1);
    expect(grid.strings.cellEditor).toBe("b");
    detach();
    grid.setStrings({ grid: "d", cellEditor: "e", selectedCell: "f" });
    expect(onStrings).toHaveBeenCalledTimes(1);
    expect(host.tabIndex).toBe(0);
    expect(host.attributes.get("role")).toBe("application");
  });

  test("leaves nothing behind", () => {
    const { grid, host } = open();
    grid.setStrings({ grid: "Cells", cellEditor: "", selectedCell: "" });
    grid.destroy();
    expect([...host.attributes.keys()]).toEqual([]);
    press(host, "ArrowDown");
    expect(grid.selection).toEqual({ row: 0, col: 0 });
  });
});

describe("the selected cell as a table", () => {
  test("has its headers and its text, which may come later", () => {
    const { grid, mirror, send, draw } = open();
    grid.setStrings({ grid: "", cellEditor: "", selectedCell: "Selected" });
    expect(mirror.attributes.get("aria-label")).toBe("Selected");
    expect(mirror.attributes.get("aria-rowcount")).toBe("10001");
    expect(mirror.attributes.get("aria-colcount")).toBe("65");
    grid.select({ row: 1, col: 0 });
    expect(mirror.cells).toEqual(["", "A", "2", "x"]);
    grid.select({ row: 400, col: 28 });
    grid.reveal(400, 28);
    expect(mirror.cells).toEqual(["", "AC", "401", ""]);
    draw();
    send();
    grid.select({ row: 400, col: 2 });
    expect(mirror.cells).toEqual(["", "C", "401", textOf(400, 2)]);
  });

  test("counts the rows a filter hides", () => {
    const { grid, mirror } = open({}, { rows: 9_990, totalRows: 10_000 });
    grid.select({ row: 3, col: 0 });
    grid.setFilterState(
      {
        headerRow: 0,
        lastRow: 99,
        firstCol: 0,
        lastCol: 3,
        activeCols: [0],
        hiddenRowSpans: [[2, 11]],
      },
      10_000,
    );
    expect(mirror.cells.slice(0, 3)).toEqual(["", "A", "14"]);
    expect(mirror.attributes.get("aria-rowcount")).toBe("10001");
    const [, body] = mirror.children;
    expect(body?.children[0]?.attributes.get("aria-rowindex")).toBe("15");
  });

  test("has the place that the selection is called by", () => {
    let name: string | undefined;
    const { grid } = open({ onSelectionChange: (selection) => (name = selection?.name) });
    grid.selectRange({ row: 1, col: 0 }, { row: 3, col: 1 });
    expect(name).toBe("A2:B4");
    const filter = { headerRow: 0, lastRow: 99, firstCol: 0, lastCol: 3, activeCols: [0] };
    grid.setFilterState({ ...filter, hiddenRowSpans: [[2, 11]] }, 10_000);
    expect(name).toBe("A2:B14");
    grid.select({ row: 2, col: 0 });
    expect(name).toBe("A13");
    grid.setFilterState(null);
    expect(name).toBe("A3");
  });

  test("has nothing in it without a selection, and an identifier of its own", () => {
    const { grid, mirror } = open();
    grid.select(null);
    expect(mirror.cells).toEqual([""]);
    expect(open().mirror.id).not.toBe(mirror.id);
  });

  test("is written to when the selection changes, not with every frame", () => {
    const { grid, mirror, draw } = open();
    grid.select({ row: 1, col: 0 });
    const write = vi.spyOn(mirror, "setAttribute");
    grid.scrollTo({ top: 1000 });
    draw();
    grid.select({ row: 1, col: 0 });
    expect(write).not.toHaveBeenCalled();
    grid.select({ row: 2, col: 0 });
    expect(write).toHaveBeenCalled();
  });
});

test("in another language everything is measured and drawn anew", () => {
  const { grid, draw } = open();
  draw();
  const { frames: before } = grid.stats();
  languageChanged();
  draw();
  expect(grid.stats().frames).toBe(before);
  document.documentElement.lang = "zh-CN";
  languageChanged();
  draw();
  expect(grid.stats().frames).toBe(before + 1);
});
