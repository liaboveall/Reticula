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
import {
  autofitSelectedCols,
  moveSelectedCols,
  moveSelectedRows,
  resizeSelectedCols,
  resizeSelectedRows,
  sizeOfSelection,
} from "./arrange.ts";
import { NamedColumns } from "./columns.ts";
import { Grid, type GridOptions, type SheetConfig, type Subscription } from "./grid.ts";
import { DEFAULT_METRICS, headerZoom } from "./layout.ts";

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
  vi.stubGlobal(
    "DOMRect",
    class {
      constructor(
        readonly x: number,
        readonly y: number,
        readonly width: number,
        readonly height: number,
      ) {}
    },
  );
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
  epoch = 1,
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
  view.setUint32(EPOCH_AT, epoch, true);
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
  const subscriptions: Subscription[] = [];
  const held = () => subscriptions.at(-1);
  const grid = new Grid(host as unknown as HTMLElement, {
    subscribe: (subscription) => subscriptions.push(subscription),
    ...options,
  });
  const send = (ids = { row: rowId, col: colId }, textAt = textOf) => {
    const { epoch, firstRow = 0, rowCount = 0, firstCol = 0, colCount = 0 } = held() ?? {};
    for (let row = firstRow / PAGE_ROWS; row * PAGE_ROWS < firstRow + rowCount; row++) {
      for (let col = firstCol / PAGE_COLS; col * PAGE_COLS < firstCol + colCount; col++) {
        grid.acceptPage(pageOf(row, col, textAt, ids, epoch));
      }
    }
  };
  const arrive = (row: number, col: number, ids = { row: rowId, col: colId }) =>
    grid.acceptPage(pageOf(row, col, textOf, ids, held()?.epoch));
  const draw = () => {
    for (const frame of frames.splice(0)) {
      frame();
    }
  };
  grid.setSheet({ rows: 10_000, cols: 64, ...sheet });
  draw();
  send();
  draw();
  const [, input, , , , mirror] = host.children;
  if (!mirror || !input) {
    throw new Error("the grid has no mirror");
  }
  return { grid, host, input, mirror, subscriptions, send, arrive, draw };
}

const cells = (firstRow: number, endRow: number, firstCol: number, endCol: number) => ({
  firstRow,
  endRow,
  firstCol,
  endCol,
});

/** What a mouse does at a place in the grid. Returns whether nothing else is to come of it. */
function point(
  input: FakeElement,
  type: string,
  clientX: number,
  clientY: number,
  init: Partial<PointerEvent> = {},
): boolean {
  const event = Object.assign(new Event(type, { cancelable: true }), {
    pointerId: 1,
    pointerType: "mouse",
    button: 0,
    clientX,
    clientY,
    shiftKey: false,
    ctrlKey: false,
    metaKey: false,
    ...init,
  });
  input.dispatchEvent(event);
  return event.defaultPrevented;
}

function drag(
  input: FakeElement,
  from: readonly [number, number],
  to: readonly [number, number],
): void {
  point(input, "pointerdown", ...from);
  point(input, "pointermove", ...to);
  point(input, "pointerup", ...to);
}

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
    expect(moveSelectedRows(grid, 1)).toBe(true);
    expect(onMoveRows).toHaveBeenLastCalledWith([{ firstId: "1004", len: 2 }], "1006");
    expect(grid.selectionRange).toEqual(cells(5, 7, 1, 3));

    // What is known of the rows is from before: the next key would move another one.
    expect(moveSelectedRows(grid, 1)).toBe(false);
    const moved = (row: number) =>
      row === 4 ? rowId(6) : row < 7 && row > 4 ? rowId(row - 1) : rowId(row);
    send({ row: moved, col: colId });
    expect(moveSelectedRows(grid, -1)).toBe(true);
    expect(onMoveRows).toHaveBeenLastCalledWith([{ firstId: "1004", len: 2 }], "1003");
    expect(grid.selectionRange).toEqual(cells(4, 6, 1, 3));
    send();

    expect(moveSelectedCols(grid, -1)).toBe(true);
    expect(onMoveCols).toHaveBeenLastCalledWith([{ firstId: "501", len: 2 }], null);
    expect(grid.selectionRange).toEqual(cells(4, 6, 0, 2));
    send();
    expect(moveSelectedCols(grid, -1)).toBe(false);
    expect(onMoveCols).toHaveBeenCalledTimes(1);
  });

  test("are moved another time only once all that is known of them is from after the first", () => {
    const onMoveRows = vi.fn();
    const { grid, arrive, send } = open({ onMoveRows });
    // In the second page across, of which the rows are known from the first.
    grid.select({ row: 4, col: PAGE_COLS + 1 });
    expect(moveSelectedRows(grid, 1)).toBe(true);
    const moved = (row: number) => (row === 4 ? rowId(5) : row === 5 ? rowId(4) : rowId(row));
    // A page that was on its way, and one that came before the one that counts.
    arrive(0, 1);
    expect(moveSelectedRows(grid, 1)).toBe(false);
    arrive(0, 1, { row: moved, col: colId });
    expect(moveSelectedRows(grid, 1)).toBe(false);
    expect(onMoveRows).toHaveBeenCalledTimes(1);
    send({ row: moved, col: colId });
    expect(moveSelectedRows(grid, 1)).toBe(true);
    expect(onMoveRows).toHaveBeenLastCalledWith([{ firstId: "1004", len: 1 }], "1006");
  });

  test("are moved another time even if nothing came of the first", () => {
    vi.useFakeTimers({ toFake: ["performance"] });
    const onMoveRows = vi.fn();
    const { grid } = open({ onMoveRows });
    grid.select({ row: 4, col: 1 });
    expect(moveSelectedRows(grid, 1)).toBe(true);
    vi.advanceTimersByTime(1_999);
    expect(moveSelectedRows(grid, 1)).toBe(false);
    vi.advanceTimersByTime(2);
    expect(moveSelectedRows(grid, 1)).toBe(true);
  });

  test("stay where they are at the edge, unknown, in several ranges, and without anybody to tell", () => {
    const onMoveRows = vi.fn();
    const { grid } = open({ onMoveRows });
    grid.select({ row: 0, col: 0 });
    expect(moveSelectedRows(grid, -1)).toBe(false);
    grid.select({ row: 9_999, col: 0 });
    expect(moveSelectedRows(grid, 1)).toBe(false);
    // Far out of view.
    grid.select({ row: 5_000, col: 0 });
    expect(moveSelectedRows(grid, 1)).toBe(false);
    grid.selectCols(1, 1);
    expect(moveSelectedRows(grid, 1)).toBe(false);
    grid.select({ row: 2, col: 2 });
    expect(moveSelectedCols(grid, 1)).toBe(false);
    grid.addSelectionRange(cells(5, 6, 0, 1));
    expect(moveSelectedRows(grid, 1)).toBe(false);
    expect(onMoveRows).not.toHaveBeenCalled();
  });

  test("get a size, in bounds, or the standard one", () => {
    const onResizeRows = vi.fn();
    const onResizeCols = vi.fn();
    const { grid } = open({ onResizeRows, onResizeCols });
    const { rowHeight, colWidth } = DEFAULT_METRICS;
    grid.selectRows(2, 3);
    expect(sizeOfSelection(grid)).toEqual({ rowPx: rowHeight, colPx: colWidth });
    expect(resizeSelectedRows(grid, 40.4)).toBe(true);
    expect(onResizeRows).toHaveBeenLastCalledWith([
      { run: { firstId: "1002", len: 2 }, sizePx: 40 },
    ]);
    expect(sizeOfSelection(grid).rowPx).toBe(40);
    // Shown without waiting for the engine.
    expect(grid.boxOf(3, 0).height).toBe(40);
    expect(grid.boxOf(4, 0).top - grid.boxOf(2, 0).top).toBe(80);
    grid.selectRows(3, 4);
    expect(sizeOfSelection(grid).rowPx).toBeNull();
    resizeSelectedRows(grid, null);
    expect(onResizeRows).toHaveBeenLastCalledWith([
      { run: { firstId: "1003", len: 2 }, sizePx: null },
    ]);
    expect(sizeOfSelection(grid).rowPx).toBe(rowHeight);
    expect(grid.boxOf(2, 0).height).toBe(40);

    grid.select({ row: 0, col: 1 });
    grid.addSelectionRange(cells(0, 1, 4, 6));
    grid.addSelectionRange(cells(3, 4, 5, 7));
    resizeSelectedCols(grid, 1);
    expect(onResizeCols).toHaveBeenLastCalledWith([
      { run: { firstId: "501", len: 1 }, sizePx: 24 },
      { run: { firstId: "504", len: 3 }, sizePx: 24 },
    ]);
    resizeSelectedCols(grid, colWidth);
    expect(onResizeCols.mock.lastCall?.[0]).toMatchObject([{ sizePx: null }, { sizePx: null }]);
    grid.selectCols(0, 0);
    expect(resizeSelectedRows(grid, 30)).toBe(false);
    expect(onResizeRows).toHaveBeenCalledTimes(2);
  });

  test("are as wide as their texts need, all at once", () => {
    const onResizeCols = vi.fn();
    const { grid } = open({ onResizeCols });
    grid.selectCols(1, 2);
    expect(autofitSelectedCols(grid)).toBe(true);
    const [sizes] = onResizeCols.mock.lastCall ?? [];
    expect(sizes).toMatchObject([{ run: { firstId: "501" } }, { run: { firstId: "502" } }]);
    expect(grid.boxOf(0, 1).width).toBe(sizes[0].sizePx);
    expect(sizes[0].sizePx).toBeLessThan(DEFAULT_METRICS.colWidth);
    expect(sizes[1].sizePx).toBeGreaterThan(7 * textOf(0, 2).length);
    // They are that wide now.
    expect(autofitSelectedCols(grid)).toBe(true);
    expect(onResizeCols).toHaveBeenCalledTimes(1);
    // Nothing is known of what is in these.
    grid.select({ row: 0, col: 60 });
    expect(autofitSelectedCols(grid)).toBe(false);
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

describe("pages that are late", () => {
  const ids = { row: rowId, col: colId };
  const old = (epoch: number) => pageOf(0, 0, () => "old", ids, epoch);
  const next = (epoch: number) => pageOf(0, 0, () => "new", ids, epoch);

  test("are not shown on the sheet that was opened since they were asked for", () => {
    const { grid, subscriptions, draw } = open();
    const before = subscriptions.at(-1)?.epoch ?? 0;
    grid.setSheet({ rows: 10_000, cols: 64 });
    draw();
    const since = subscriptions.at(-1);
    expect(since).toMatchObject({ resend: true });
    expect(since?.epoch).not.toBe(before);
    const { pagesDiscarded } = grid.stats();
    grid.acceptPage(old(before));
    expect(grid.textAt(0, 0)).toBeNull();
    expect(grid.stats().pagesDiscarded).toBe(pagesDiscarded + 1);
    grid.acceptPage(next(since?.epoch ?? 0));
    expect(grid.textAt(0, 0)).toBe("new");
    grid.acceptPage(old(before));
    expect(grid.textAt(0, 0)).toBe("new");
  });

  test("are shown if the same sheet has only been scrolled since", () => {
    const { grid, subscriptions, draw } = open({ margin: { rows: 0, cols: 0 } });
    const before = subscriptions.at(-1)?.epoch ?? 0;
    grid.scrollTo({ top: 24 * 50 });
    draw();
    expect(subscriptions.at(-1)?.epoch).not.toBe(before);
    expect(subscriptions.at(-1)).toMatchObject({ resend: false });
    grid.acceptPage(old(before));
    expect(grid.textAt(10, 0)).toBe("old");
  });
});

describe("a grid that is paused", () => {
  test("asks for nothing and draws nothing, until it is resumed", () => {
    const { grid, subscriptions, send, draw } = open();
    expect(grid.textAt(1, 0)).toBe("x");
    const asked = subscriptions.length;
    grid.pause();
    expect(subscriptions.slice(asked)).toMatchObject([{ rowCount: 0, colCount: 0 }]);
    expect(grid.stats().pagesHeld).toBe(0);
    const { frames: drawn } = grid.stats();
    grid.pause();
    grid.scrollTo({ top: 24 * 1_000 });
    grid.updateSheet({ rows: 9_000, cols: 64 });
    draw();
    expect(subscriptions).toHaveLength(asked + 1);
    expect(grid.stats().frames).toBe(drawn);

    grid.resume();
    draw();
    expect(grid.stats().frames).toBe(drawn + 1);
    expect(subscriptions).toHaveLength(asked + 2);
    expect(subscriptions.at(-1)).toMatchObject({ resend: true });
    expect(subscriptions.at(-1)?.firstRow).toBeGreaterThan(0);
    send();
    expect(grid.textAt(1_001, 2)).toBe(textOf(1_001, 2));
  });

  test("takes no page that was on its way when it was paused", () => {
    const { grid, subscriptions, draw } = open();
    const before = subscriptions.at(-1)?.epoch;
    const late = () =>
      grid.acceptPage(pageOf(0, 0, () => "old", { row: rowId, col: colId }, before));
    grid.pause();
    late();
    expect(grid.stats().pagesHeld).toBe(0);
    grid.resume();
    draw();
    late();
    expect(grid.textAt(0, 0)).toBeNull();
  });

  test("opens sheets all the same", () => {
    const { grid, subscriptions, draw } = open();
    grid.pause();
    const asked = subscriptions.length;
    grid.setSheet({ rows: 500, cols: 8 }, { row: 100 });
    grid.refresh({ rows: 400, cols: 8 }, { row: 90 });
    draw();
    expect(subscriptions).toHaveLength(asked);
    grid.resume();
    draw();
    expect(subscriptions.at(-1)).toMatchObject({ resend: true });
    expect(grid.stats().visible.firstRow).toBe(90);
  });
});

describe("a sheet whose rows are others now", () => {
  const ids = { row: rowId, col: colId };

  test("is shown with what there is of it, all in one frame", () => {
    const { grid, subscriptions, send, draw } = open();
    grid.scrollTo({ top: 24 * 5_000, left: 300 });
    draw();
    send();
    draw();
    grid.select({ row: 5_010, col: 4 });
    const before = subscriptions.at(-1)?.epoch ?? 0;
    const { framesWithMissing } = grid.stats();
    // Encoded before the grid could ask for it.
    const pages = [0, 1].map((col) => pageOf(0, col, () => "new", ids, before));
    grid.refresh({ rows: 700, cols: 64 }, {}, pages);
    expect(subscriptions.at(-1)).toMatchObject({ firstRow: 0, firstCol: 0, resend: true });
    expect(subscriptions.at(-1)?.epoch).toBe(before + 1);
    const asked = subscriptions.length;
    draw();
    expect(grid.textAt(0, 0)).toBe("new");
    expect(grid.stats()).toMatchObject({ framesWithMissing, scroll: { top: 0, left: 0 } });
    expect(grid.sheetSize.rows).toBe(700);
    expect(grid.selection).toEqual({ row: 0, col: 0 });
    expect(subscriptions).toHaveLength(asked);

    // What is sent for the rows as they were is not shown any more.
    grid.acceptPage(pageOf(0, 0, () => "old", ids, before));
    expect(grid.textAt(0, 0)).toBe("new");
    grid.acceptPage(pageOf(0, 0, () => "newer", ids, before + 1));
    expect(grid.textAt(0, 0)).toBe("newer");
  });

  test("stays where it is across, if that is asked for", () => {
    const { grid, draw } = open();
    grid.scrollTo({ top: 24 * 5_000, left: 112 * 6 + 10 });
    draw();
    const { col, colFraction } = grid.view();
    const cell = { row: 0, col: 9 };
    grid.refresh(
      { rows: 700, cols: 64 },
      { col, colFraction, selection: { anchor: cell, focus: cell, scope: "cells" } },
    );
    draw();
    expect(grid.stats().scroll).toEqual({ top: 0, left: 112 * 6 + 10 });
    expect(grid.selection).toEqual(cell);
  });

  test("may have no rows at all, and have some again", () => {
    const onSelectionChange = vi.fn();
    const { grid, input, subscriptions, draw, send } = open({ onSelectionChange });
    const before = subscriptions.at(-1)?.epoch ?? 0;
    grid.refresh({ rows: 0, cols: 64, totalRows: 10_000 });
    draw();
    expect(subscriptions.at(-1)).toMatchObject({ rowCount: 0, resend: true });
    expect(subscriptions.at(-1)?.epoch).toBe(before + 1);
    expect(grid.selection).toBeNull();
    expect(onSelectionChange).toHaveBeenLastCalledWith(null);
    // A click on a header selects none of it.
    point(input, "pointerdown", 100, 10);
    point(input, "pointerup", 100, 10);
    expect(grid.selection).toBeNull();
    grid.refresh({ rows: 10, cols: 64 });
    draw();
    send();
    expect(grid.selection).toEqual({ row: 0, col: 0 });
    expect(grid.textAt(1, 0)).toBe("x");
  });
});

describe("columns with names", () => {
  const columns = new NamedColumns(
    Array.from({ length: 64 }, (_, col) => ({
      name: col === 1 ? "unit price" : `c${col}`,
      sort: col === 1 ? ("descending" as const) : null,
    })),
  );

  test("are called by them", () => {
    const { grid, mirror } = open({}, { columns });
    grid.select({ row: 1, col: 1 });
    expect(mirror.cells).toEqual(["", "unit price", "2", "x"]);
    const header = () => mirror.children[0]?.children[0]?.children[1];
    expect(header()).toMatchObject({ ariaSort: "descending" });
    expect(grid.selectionDetails()?.name).toBe("unit price 2");
    grid.selectRange({ row: 1, col: 0 }, { row: 2, col: 1 });
    expect(header()).toMatchObject({ ariaSort: null });
    expect(grid.selectionDetails()?.name).toBe("c0 2:unit price 3");

    // Other names, and letters.
    grid.updateSheet({ rows: 10_000, cols: 64, columns: new NamedColumns([{ name: "z" }]) });
    expect(mirror.cells.slice(0, 2)).toEqual(["", "z"]);
    grid.updateSheet({ rows: 10_000, cols: 64 });
    expect(mirror.cells.slice(0, 2)).toEqual(["", "z"]);
    grid.updateSheet({ rows: 10_000, cols: 64, columns: null });
    expect(mirror.cells.slice(0, 2)).toEqual(["", "A"]);
    expect(grid.selectionDetails()?.name).toBe("A2:B3");
  });

  test("are made wide enough for them", () => {
    const onResizeCols = vi.fn();
    const long = new NamedColumns([{ name: "a name much longer than all the texts below it" }]);
    const { grid } = open({ onResizeCols }, { columns: long });
    grid.select({ row: 0, col: 0 });
    autofitSelectedCols(grid);
    const wide = grid.boxOf(0, 0).width;
    expect(wide).toBeGreaterThan(7 * long.nameOf(0).length + long.roomOf(0));
    grid.updateSheet({ rows: 10_000, cols: 64, columns: null });
    autofitSelectedCols(grid);
    expect(grid.boxOf(0, 0).width).toBeLessThan(wide);
  });

  test("have a button each, which selects nothing", () => {
    const onColumnButton = vi.fn();
    const { grid, input } = open({ onColumnButton }, { columns });
    grid.select({ row: 7, col: 5 });
    // The second column, from 176 to 288.
    const button = { x: 288 - 5 - 16, y: 4, width: 16, height: 16 };
    for (const [x, y] of [
      [270, 12],
      [282, 2],
      [260, 23],
    ] as const) {
      onColumnButton.mockClear();
      expect(point(input, "pointerdown", x, y)).toBe(true);
      expect(onColumnButton).toHaveBeenCalledExactlyOnceWith(1, expect.objectContaining(button));
      point(input, "pointerup", x, y);
    }
    expect(grid.selection).toEqual({ row: 7, col: 5 });
    expect(grid.selectionScope).toBe("cells");

    // Not the rest of the header, the edge that the column is resized by, or a cell.
    onColumnButton.mockClear();
    for (const [x, y] of [
      [200, 12],
      [286, 12],
      [270, 30],
    ] as const) {
      point(input, "pointerdown", x, y);
      point(input, "pointerup", x, y);
    }
    expect(onColumnButton).not.toHaveBeenCalled();
    expect(grid.columnButtonRect(1)).toMatchObject(button);
  });

  test("have their buttons where they are scrolled to, zoomed to and frozen", () => {
    const onColumnButton = vi.fn();
    const { grid, input, draw } = open({ onColumnButton }, { columns, frozenCols: 1 });
    grid.scrollTo({ left: 112 * 3 + 50 });
    draw();
    point(input, "pointerdown", 64 + 112 - 12, 12);
    expect(onColumnButton.mock.lastCall?.[0]).toBe(0);
    // The fifth column ends at 64 + 5 * 112 - 386.
    point(input, "pointerdown", 64 + 560 - 386 - 12, 12);
    expect(onColumnButton.mock.lastCall?.[0]).toBe(4);
    // One that has no room for a button has its header.
    grid.updateSheet({ rows: 10_000, cols: 64, customCols: [[4, 30]] });
    draw();
    const narrow = { x: 64 + 448 - 386, y: 0, width: 30, height: 24 };
    expect(grid.columnButtonRect(4)).toMatchObject(narrow);
    onColumnButton.mockClear();
    point(input, "pointerdown", narrow.x + 15, 12);
    point(input, "pointerup", narrow.x + 15, 12);
    expect(onColumnButton).not.toHaveBeenCalled();
    grid.updateSheet({ rows: 10_000, cols: 64, customCols: [] });
    draw();
    for (const zoom of [0.5, 0.75, 1.5, 2]) {
      grid.setZoom(zoom);
      draw();
      const { headerHeight } = grid.stats().metrics;
      const rect = grid.columnButtonRect(4);
      expect(rect.width).toBeCloseTo(16 * headerZoom(zoom), 9);
      onColumnButton.mockClear();
      point(input, "pointerdown", rect.x + 3, headerHeight / 2);
      expect(onColumnButton.mock.lastCall?.[0]).toBe(4);
    }
  });

  test("have no buttons that do anything without anybody to tell", () => {
    const { grid, input } = open({}, { columns });
    point(input, "pointerdown", 270, 12);
    point(input, "pointerup", 270, 12);
    expect(grid.selectionScope).toBe("cols");
    const none = open({ onColumnButton: vi.fn() });
    expect(point(none.input, "pointerdown", 270, 12)).toBe(false);
    expect(none.grid.selectionScope).toBe("cols");
  });

  test("have their buttons when there are no rows", () => {
    const onColumnButton = vi.fn();
    const { grid, input, draw } = open({ onColumnButton }, { columns });
    grid.refresh({ rows: 0, cols: 64, totalRows: 10_000, columns });
    draw();
    expect(point(input, "pointerdown", 270, 12)).toBe(true);
    expect(onColumnButton.mock.lastCall?.[0]).toBe(1);
  });
});

describe("a grid to read in", () => {
  const callbacks = () => ({
    readOnly: true,
    onResizeRows: vi.fn(),
    onResizeCols: vi.fn(),
    onMoveRows: vi.fn(),
    onMoveCols: vi.fn(),
    onFillRange: vi.fn(),
    onAutoFillDown: vi.fn(),
    onLocalColumnResize: vi.fn(),
  });
  const expectNothing = ({
    readOnly,
    onLocalColumnResize,
    ...others
  }: ReturnType<typeof callbacks>) => {
    for (const callback of Object.values(others)) {
      expect(callback).not.toHaveBeenCalled();
    }
  };

  test("has no handle to fill with", () => {
    const options = callbacks();
    const { grid, input } = open(options);
    grid.select({ row: 2, col: 1 });
    const corner = [64 + 224, 24 + 72] as const;
    point(input, "pointermove", ...corner);
    expect(input.style).toMatchObject({ cursor: "cell" });
    drag(input, corner, [corner[0], corner[1] + 100]);
    point(input, "dblclick", ...corner);
    expectNothing(options);
    const other = open({ onFillRange: options.onFillRange });
    other.grid.select({ row: 2, col: 1 });
    drag(other.input, corner, [corner[0], corner[1] + 100]);
    expect(options.onFillRange).toHaveBeenCalledTimes(1);
  });

  test("has rows and columns that stay in their order, and rows of one height", () => {
    const options = callbacks();
    const { grid, input } = open(options);
    const across = (from: readonly [number, number], to: readonly [number, number]) => {
      point(input, "pointermove", ...from);
      expect(input.style).toMatchObject({ cursor: "pointer" });
      drag(input, from, to);
    };
    grid.selectCols(1, 1);
    across([200, 12], [500, 12]);
    expect(grid.selectionRange).toEqual(cells(0, 10_000, 1, 4));
    grid.selectRows(2, 2);
    across([20, 84], [20, 300]);
    // The whole rows that were dragged across are selected instead.
    expect(grid.selectionScope).toBe("rows");
    expect(grid.selectionRange?.endRow).toBeGreaterThan(10);

    const height = grid.boxOf(2, 0).height;
    const edge = [20, 24 + 72] as const;
    point(input, "pointermove", ...edge);
    expect(input.style).toMatchObject({ cursor: "pointer" });
    drag(input, edge, [20, edge[1] + 40]);
    expect(grid.boxOf(2, 0).height).toBe(height);
    grid.selectRows(1, 2);
    expect(resizeSelectedRows(grid, 60)).toBe(false);
    expect(moveSelectedRows(grid, 1)).toBe(false);
    grid.selectCols(1, 1);
    expect(moveSelectedCols(grid, 1)).toBe(false);
    expectNothing(options);
  });

  test("has columns that can be made wider, which only it knows of", () => {
    const options = callbacks();
    const { grid, input } = open(options);
    const { onLocalColumnResize } = options;
    drag(input, [64 + 224 - 1, 12], [64 + 224 + 49, 12]);
    expect(onLocalColumnResize).toHaveBeenLastCalledWith([{ col: 1, sizePx: 162 }]);
    expect(grid.boxOf(0, 1).width).toBe(162);
    grid.selectCols(3, 4);
    expect(resizeSelectedCols(grid, 50)).toBe(true);
    expect(onLocalColumnResize).toHaveBeenLastCalledWith([
      { col: 3, sizePx: 50 },
      { col: 4, sizePx: 50 },
    ]);
    expect(resizeSelectedCols(grid, null)).toBe(true);
    expect(onLocalColumnResize).toHaveBeenLastCalledWith([
      { col: 3, sizePx: null },
      { col: 4, sizePx: null },
    ]);
    grid.selectCols(2, 2);
    expect(autofitSelectedCols(grid)).toBe(true);
    const [[fit] = []] = onLocalColumnResize.mock.lastCall ?? [];
    expect(fit).toMatchObject({ col: 2 });
    expect(grid.boxOf(0, 2).width).toBe(fit.sizePx);
    // Back to what its text needs, by its edge.
    resizeSelectedCols(grid, 40);
    point(input, "dblclick", grid.boxOf(0, 2).left + 39, 12);
    expect(onLocalColumnResize).toHaveBeenLastCalledWith([fit]);
    expectNothing(options);
  });

  test("has columns that can be resized before anything is known of them", () => {
    const options = callbacks();
    const { grid, input, draw } = open(options);
    grid.refresh({ rows: 0, cols: 64 });
    draw();
    drag(input, [64 + 112 - 1, 12], [64 + 112 + 9, 12]);
    expect(options.onLocalColumnResize).toHaveBeenLastCalledWith([{ col: 0, sizePx: 122 }]);
  });

  test("opens menus, and selects cells, rows and columns", () => {
    const onContextMenu = vi.fn();
    const { grid, input, host } = open({ ...callbacks(), onContextMenu });
    point(input, "contextmenu", 200, 12);
    expect(onContextMenu).toHaveBeenLastCalledWith(
      expect.objectContaining({ zone: "colHeader", col: 1 }),
    );
    expect(grid.selectionScope).toBe("cols");
    // The edge of a row is a part of it.
    point(input, "contextmenu", 20, 24 + 72);
    expect(onContextMenu).toHaveBeenLastCalledWith(
      expect.objectContaining({ zone: "rowHeader", row: 3 }),
    );
    drag(input, [100, 40], [400, 100]);
    expect(grid.selectionRange).toEqual(cells(0, 4, 0, 4));
    expect(press(host, "ArrowDown", { ctrlKey: true, shiftKey: true })).toBe(true);
    expect(grid.selectionRange).toEqual(cells(0, 10_000, 0, 4));
  });
});

describe("row headers", () => {
  const widthAt = (rows: number, totalRows?: number) => {
    const { grid } = open({}, { rows, ...(totalRows === undefined ? {} : { totalRows }) });
    return grid.stats().metrics.headerWidth;
  };

  test("are as wide as ever up to eight digits, and wider with more", () => {
    expect([1, 2_000_000, 99_999_999].map((rows) => widthAt(rows))).toEqual([64, 64, 64]);
    expect(widthAt(100_000_000)).toBe(72);
  });

  test("have room for the numbers of all rows, however few are left", () => {
    expect(widthAt(12, 100_000_000)).toBe(72);
    const { grid, draw } = open({}, { rows: 100_000_000, totalRows: 100_000_000 });
    grid.refresh({ rows: 3, cols: 64, totalRows: 100_000_000 });
    draw();
    expect(grid.boxOf(0, 0).left).toBe(72);
    grid.setZoom(0.5);
    draw();
    expect(grid.stats().metrics.headerWidth).toBe(54);
    grid.updateSheet({ rows: 3, cols: 64, totalRows: 3 });
    expect(grid.stats().metrics.headerWidth).toBe(48);
    grid.setZoom(1);
    expect(grid.boxOf(0, 0).left).toBe(64);
  });
});

describe("a hundred million rows", () => {
  const rows = 100_000_000;

  test("are gone through with the keys, to the last one", () => {
    const { grid, host, mirror, draw, send } = open({ readOnly: true }, { rows, totalRows: rows });
    expect(press(host, "ArrowDown", { ctrlKey: true })).toBe(true);
    draw();
    send();
    expect(grid.selection).toEqual({ row: rows - 1, col: 0 });
    expect(grid.stats().visible.endRow).toBe(rows);
    expect(grid.stats().scroll.top).toBe(grid.stats().scrollLimits.top);
    expect(grid.textAt(rows - 1, 2)).toBe(textOf(rows - 1, 2));
    expect(mirror.cells.slice(0, 3)).toEqual(["", "A", "100000000"]);
    expect(mirror.attributes.get("aria-rowcount")).toBe("100000001");
    expect(grid.selectionDetails()?.name).toBe("A100000000");
    press(host, "PageUp");
    press(host, "PageDown");
    expect(grid.selection).toEqual({ row: rows - 1, col: 0 });
    press(host, "ArrowUp");
    expect(grid.selection).toEqual({ row: rows - 2, col: 0 });
    press(host, "End");
    expect(grid.selection).toEqual({ row: rows - 2, col: 63 });
    press(host, "Home", { ctrlKey: true, shiftKey: true });
    expect(grid.selectionRange).toEqual(cells(0, rows - 1, 0, 64));
    press(host, "Home", { ctrlKey: true });
    draw();
    expect(grid.stats().scroll).toEqual({ top: 0, left: 0 });
    grid.selectCols(2, 2);
    expect(grid.selectionRange).toEqual(cells(0, rows, 2, 3));
  });

  test("are jumped to and pointed at one by one", () => {
    const { grid, input, draw } = open({ readOnly: true }, { rows, totalRows: rows });
    for (const row of [rows - 1, rows - 20, 99_999_937, 54_321_098, 10_000_001]) {
      grid.jumpTo(row, 3, false);
      draw();
      const { left, top, height } = grid.boxOf(row, 3);
      expect(height).toBe(24);
      point(input, "pointerdown", left + 20, top + height / 2);
      point(input, "pointerup", left + 20, top + height / 2);
      expect(grid.selection).toEqual({ row, col: 3 });
      point(input, "pointerdown", 20, top + height / 2);
      point(input, "pointerup", 20, top + height / 2);
      expect(grid.selectionRange).toEqual(cells(row, row + 1, 0, 64));
    }
  });

  test("are where they were when their sheet is opened again", () => {
    const { grid, draw } = open({}, { rows, totalRows: rows });
    grid.jumpTo(87_654_321, 5, false);
    draw();
    const view = grid.view();
    const { scroll } = grid.stats();
    grid.setSheet({ rows, cols: 64, totalRows: rows });
    draw();
    grid.setSheet({ rows, cols: 64, totalRows: rows }, view);
    draw();
    expect(grid.stats().scroll).toEqual(scroll);
    expect(grid.selection).toEqual({ row: 87_654_321, col: 5 });
  });
});
