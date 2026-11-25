import type { Selection, SelectionScope, Span } from "@reticula/grid";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Activity, SheetAction } from "./activity.ts";
import {
  CellAnnouncer,
  type CellInspection,
  cellText,
  filterText,
  outcomeText,
  plainSelectionText,
  selectionText,
} from "./cell-announcer.ts";

// What is loaded later may arrive when the test is over.
vi.mock("../i18n/zh-engine.ts", () => ({ default: { "Edit {cell}": "编辑 {cell}" } }));
vi.mock("../i18n/zh-functions.ts", () => ({ default: {} }));
vi.mock("../i18n/zh-tables.ts", () => ({ default: {} }));

async function inLanguage(search: string): Promise<void> {
  const { initLocale } = await import("../i18n/index.ts");
  vi.stubGlobal("window", {
    location: { search },
    localStorage: { getItem: () => null },
    navigator: { languages: [] },
  });
  await initLocale("app");
  // What is loaded later arrives before the test is over.
  await vi.dynamicImportSettled();
}

beforeEach(() => {
  vi.stubGlobal("document", { documentElement: {} });
});

afterEach(async () => {
  await inLanguage("?lang=en");
  vi.unstubAllGlobals();
});

const inspection = (changes: Partial<CellInspection>): CellInspection => ({
  address: "B2",
  raw: "",
  display: "",
  isFormula: false,
  error: null,
  valueKind: "empty",
  spillAnchor: null,
  ...changes,
});

const span = (firstRow: number, endRow: number, firstCol: number, endCol: number): Span => ({
  firstRow,
  endRow,
  firstCol,
  endCol,
});

function selection(range: Span, scope: SelectionScope = "cells", others: Span[] = []): Selection {
  return {
    row: range.firstRow,
    col: range.firstCol,
    range,
    ranges: [...others, range],
    scope,
    name: "",
    rowId: String(range.firstRow),
    colId: String(range.firstCol),
    text: null,
  };
}

const CELLS: readonly (readonly [Partial<CellInspection>, string, string])[] = [
  [{ raw: "1234.5", display: "1,234.50", valueKind: "number" }, "B2, 1,234.50", "B2，1,234.50"],
  [
    { raw: "=SUM(B3:B9)", display: "1,234.50", isFormula: true, valueKind: "number" },
    "B2, 1,234.50, formula =SUM(B3:B9)",
    "B2，1,234.50，公式 =SUM(B3:B9)",
  ],
  [
    { raw: "=A1/0", display: "#DIV/0!", isFormula: true, error: "#DIV/0!", valueKind: "error" },
    "B2, error #DIV/0!, formula =A1/0",
    "B2，错误 #DIV/0!，公式 =A1/0",
  ],
  [
    {
      address: "B3",
      display: "2",
      valueKind: "number",
      spillAnchor: { address: "B2", formula: "=SEQUENCE(3)" },
    },
    "B3, 2, spilled from B2, formula =SEQUENCE(3)",
    "B3，2，由 B2 溢出，公式 =SEQUENCE(3)",
  ],
  [{}, "B2, blank", "B2，空"],
  [
    { raw: '=""', isFormula: true, valueKind: "text" },
    'B2, empty text, formula =""',
    'B2，空文本，公式 =""',
  ],
  [
    { raw: "x".repeat(200), display: "x".repeat(200), valueKind: "text" },
    `B2, ${"x".repeat(200)}`,
    `B2，${"x".repeat(200)}`,
  ],
  [
    { raw: "", display: "x".repeat(1_300), valueKind: "text" },
    `B2, ${"x".repeat(200)}, and 1,100 more characters`,
    `B2，${"x".repeat(200)}，另有 1,100 个字符`,
  ],
];

const RANGES: readonly (readonly [Selection, string, string])[] = [
  [selection(span(0, 5, 0, 3)), "A1 to C5 selected, 15 cells", "已选中 A1 到 C5，共 15 个单元格"],
  [selection(span(4, 5, 0, 22), "rows"), "Row 5 selected", "已选中第 5 行"],
  [
    selection(span(4, 2_004, 0, 22), "rows"),
    "Rows 5 to 2004 selected, 2,000 rows",
    "已选中第 5 到 2004 行，共 2,000 行",
  ],
  [selection(span(0, 99, 1, 2), "cols"), "Column B selected", "已选中 B 列"],
  [
    selection(span(0, 99, 1, 28), "cols"),
    "Columns B to AB selected, 27 columns",
    "已选中 B 到 AB 列，共 27 列",
  ],
  [selection(span(0, 99, 0, 22), "all"), "All cells selected", "已全选"],
  [
    selection(span(3, 4, 3, 4), "cells", [span(0, 1, 0, 2), span(5, 6, 0, 1)]),
    "D4 selected, 3 ranges selected",
    "已选中 D4，共选中 3 个区域",
  ],
];

const own = (row: number) => row + 1;

describe("what is read of the selection", () => {
  it("is the cell, what it shows and where that comes from", async () => {
    expect(CELLS.map(([cell]) => cellText(inspection(cell)))).toEqual(
      CELLS.map(([, english]) => english),
    );
    await inLanguage("?lang=zh");
    expect(CELLS.map(([cell]) => cellText(inspection(cell)))).toEqual(
      CELLS.map(([, , chinese]) => chinese),
    );
  });

  it("is where a range begins and ends, and how many there are", async () => {
    expect(RANGES.map(([range]) => selectionText(range, null, own))).toEqual(
      RANGES.map(([, english]) => english),
    );
    await inLanguage("?lang=zh");
    expect(RANGES.map(([range]) => selectionText(range, null, own))).toEqual(
      RANGES.map(([, , chinese]) => chinese),
    );
  });

  it("cuts no character in half", () => {
    const display = `${"x".repeat(199)}\u{1f600}${"x".repeat(20)}`;
    expect(cellText(inspection({ display, valueKind: "text" }))).toBe(
      `B2, ${"x".repeat(199)}, and 22 more characters`,
    );
  });

  it("waits for what the engine says of a cell, and not of a range", () => {
    expect(selectionText(selection(span(1, 2, 1, 2)), null, own)).toBeNull();
    expect(selectionText(selection(span(1, 3, 1, 2)), null, own)).toBe(
      "B2 to B3 selected, 2 cells",
    );
  });

  it("goes by the rows of the sheet, not by those a filter shows", () => {
    // Rows 2 to 10 are hidden.
    const filtered = (row: number) => (row === 0 ? 1 : row + 10);
    expect(selectionText(selection(span(0, 3, 0, 1)), null, filtered)).toBe(
      "A1 to A12 selected, 3 cells",
    );
    expect(selectionText(selection(span(1, 2, 0, 9), "rows"), null, filtered)).toBe(
      "Row 11 selected",
    );
  });

  it("is what the grid shows where there is no engine", () => {
    const cell = selection(span(1, 2, 1, 2));
    expect(plainSelectionText({ ...cell, text: "7" })).toBe("B2, 7");
    expect(plainSelectionText(cell)).toBe("B2, blank");
    expect(plainSelectionText(selection(span(0, 2, 0, 1)))).toBe("A1 to A2 selected, 2 cells");
  });
});

describe("the selection", () => {
  function reader() {
    const said: string[] = [];
    return { announcer: new CellAnnouncer((text) => said.push(text)), said };
  }
  const cell = selection(span(1, 2, 1, 2));
  const five = inspection({ raw: "5", display: "5.00", valueKind: "number" });

  it("is read once the engine has said what there is, and once only", () => {
    const { announcer, said } = reader();
    announcer.show(0, cell, null, own);
    expect(said).toEqual([]);
    announcer.show(0, cell, five, own);
    // Pages arrive, and the engine is asked again after every recalculation.
    announcer.show(0, { ...cell }, null, own);
    announcer.show(0, { ...cell }, { ...five }, own);
    expect(said).toEqual(["B2, 5.00"]);
  });

  it("is read again when its content changes, or on another sheet, or when asked to", () => {
    const { announcer, said } = reader();
    announcer.show(0, cell, five, own);
    announcer.show(0, cell, { ...five, display: "6.00", raw: "6" }, own);
    // It shows the same and is another number.
    announcer.show(0, cell, { ...five, display: "6.00", raw: "6.001" }, own);
    announcer.show(1, cell, { ...five, display: "6.00", raw: "6.001" }, own);
    announcer.forget();
    announcer.show(1, cell, { ...five, display: "6.00", raw: "6.001" }, own);
    expect(said).toEqual(["B2, 5.00", "B2, 6.00", "B2, 6.00", "B2, 6.00", "B2, 6.00"]);
  });

  it("is read when it comes back to a cell from a range", () => {
    const { announcer, said } = reader();
    const range = selection(span(1, 3, 1, 2));
    announcer.show(0, cell, five, own);
    announcer.show(0, range, five, own);
    // What is in one of its cells is not read, and neither is that it changes.
    announcer.show(0, range, { ...five, raw: "6" }, own);
    announcer.show(0, cell, five, own);
    expect(said).toEqual(["B2, 5.00", "B2 to B3 selected, 2 cells", "B2, 5.00"]);
  });
});

describe("what an action has come to", () => {
  const sheet = {
    sheet: 0,
    sheetName: "Data",
    rows: 100,
    totalRows: 100,
    filter: undefined,
    undoLabel: "Edit B2",
    redoLabel: undefined,
  };
  const done = (action: SheetAction): Activity => ({
    kind: "done",
    action,
    ms: 1,
    recalculated: 5,
  });

  it("is said by the name it has in the list of what can be undone", async () => {
    const undone = { ...sheet, undoLabel: undefined, redoLabel: "Edit B2" };
    expect(outcomeText(done("undidAction"), sheet, undone)).toBe("Undid: Edit B2");
    expect(outcomeText(done("redidAction"), undone, sheet)).toBe("Redid: Edit B2");
    // There was nothing to undo, or to redo.
    expect(outcomeText(done("undidAction"), undone, undone)).toBeNull();
    expect(outcomeText(done("redidAction"), sheet, sheet)).toBeNull();
    const sorted = { ...sheet, undoLabel: "Sort" };
    expect(outcomeText(done("updated"), sheet, sorted)).toBe("Done: Sort");
    expect(outcomeText(done("insertedRows"), sheet, sorted)).toBe("inserted rows");
  });

  it("is not said of what others do, of recalculation, and of what is read otherwise", () => {
    expect(outcomeText(done("updated"), sheet, sheet)).toBeNull();
    expect(outcomeText(done("updatedCell"), sheet, { ...sheet, undoLabel: "Edit B3" })).toBeNull();
    expect(outcomeText(done("switchedSheet"), sheet, sheet)).toBeNull();
    expect(outcomeText({ kind: "recalculating", done: 1, total: 9 }, sheet, sheet)).toBeNull();
    expect(outcomeText({ kind: "recalculated", cells: 9, ms: 1 }, sheet, sheet)).toBeNull();
    expect(outcomeText({ kind: "replaced", cells: 2 }, sheet, sheet)).toBeNull();
  });

  it("is how many rows a filter shows", async () => {
    const on = { ...sheet, filter: { activeCols: [] } };
    const some = { ...on, rows: 12, filter: { activeCols: [1] } };
    const same = { ...some, filter: { activeCols: [1, 2] } };
    expect(filterText(sheet, sheet)).toBeNull();
    expect(filterText(sheet, on)).toBe("Filter on");
    expect(filterText(on, some)).toBe("12 of 100 rows shown");
    expect(filterText(some, { ...some })).toBeNull();
    expect(filterText(some, same)).toBe("12 of 100 rows shown");
    expect(filterText(some, sheet)).toBe("Filter off, all 100 rows shown");
    expect(filterText(on, sheet)).toBe("Filter off");
    // Rows are inserted and the filter goes on.
    expect(filterText(some, { ...some, rows: 13, totalRows: 101 })).toBeNull();
    await inLanguage("?lang=zh");
    expect(filterText(on, some)).toBe("显示 100 行中的 12 行");
    expect(filterText(some, sheet)).toBe("已关闭筛选，显示全部 100 行");
  });
});
