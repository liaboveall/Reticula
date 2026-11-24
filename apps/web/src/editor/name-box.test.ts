import type { Grid, Span } from "@reticula/grid";
import type { SheetInfo, WireNamedRangeInfo } from "@reticula/protocol";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { EditorHandle } from "../commands/handle.ts";
import type { EngineMessage } from "../engine/client.ts";
import { type Area, goTo, parseReference, shownRows } from "./name-box.ts";

const cells = (firstRow: number, endRow: number, firstCol: number, endCol: number): Area => ({
  scope: "cells",
  firstRow,
  endRow,
  firstCol,
  endCol,
});

const rows = (firstRow: number, endRow: number): Area => ({
  ...cells(firstRow, endRow, 0, 1),
  scope: "rows",
});

const cols = (firstCol: number, endCol: number): Area => ({
  ...cells(0, 1, firstCol, endCol),
  scope: "cols",
});

describe("a reference", () => {
  it.each<[string, readonly Area[]]>([
    ["B200", [cells(199, 200, 1, 2)]],
    ["b200", [cells(199, 200, 1, 2)]],
    ["$B$200", [cells(199, 200, 1, 2)]],
    ["AA1", [cells(0, 1, 26, 27)]],
    ["A1:C5", [cells(0, 5, 0, 3)]],
    ["C5:A1", [cells(0, 5, 0, 3)]],
    ["$A1 : C$5", [cells(0, 5, 0, 3)]],
    ["5:5", [rows(4, 5)]],
    ["9:7", [rows(6, 9)]],
    ["$5:$6", [rows(4, 6)]],
    ["B:B", [cols(1, 2)]],
    ["d:b", [cols(1, 4)]],
    ["A1:B2,D4", [cells(0, 2, 0, 2), cells(3, 4, 3, 4)]],
    ["A1:B2; 5:5 , B:B", [cells(0, 2, 0, 2), rows(4, 5), cols(1, 2)]],
    ["A1，C3", [cells(0, 1, 0, 1), cells(2, 3, 2, 3)]],
  ])("%s", (text, areas) => {
    expect(parseReference(text)).toEqual({ sheet: null, areas });
  });

  it.each<[string, string]>([
    ["Sheet2!A1", "Sheet2"],
    ["'My sheet'!A1", "My sheet"],
    ["'It''s'!A1", "It's"],
    ["'a, b'!A1", "a, b"],
    ["'a!b'!A1", "a!b"],
    ["销售!A1", "销售"],
  ])("%s", (text, sheet) => {
    expect(parseReference(text)).toEqual({ sheet, areas: [cells(0, 1, 0, 1)] });
  });

  it("may say the sheet more than once, as long as it is the same", () => {
    const areas = [cells(0, 1, 0, 1), cells(1, 2, 1, 2)];
    expect(parseReference("Sheet2!A1,B2")).toEqual({ sheet: "Sheet2", areas });
    expect(parseReference("Sheet2!A1,Sheet2!B2")).toEqual({ sheet: "Sheet2", areas });
    expect(parseReference("Sheet2!A1,Sheet3!B2")).toBeNull();
    expect(parseReference("A1,Sheet3!B2")).toBeNull();
  });

  it.each([
    "",
    " ",
    "B",
    "5",
    "A0",
    "A1:",
    ":B2",
    "A1:B",
    "5:B",
    "AAAA1",
    "A1 B2",
    "TAX_RATE",
    "=A1",
    "'Sheet2!A1",
    "Sheet2!",
    "!A1",
    "A99999999999",
  ])("is not %j", (text) => {
    expect(parseReference(text)).toBeNull();
  });
});

describe("rows under a filter", () => {
  it("are found by their numbers", () => {
    const hidden = [
      [2, 4],
      [8, 8],
    ] as const;
    expect(shownRows({ firstRow: 0, endRow: 1 }, hidden)).toEqual({ firstRow: 0, endRow: 1 });
    expect(shownRows({ firstRow: 5, endRow: 6 }, hidden)).toEqual({ firstRow: 2, endRow: 3 });
    expect(shownRows({ firstRow: 9, endRow: 10 }, hidden)).toEqual({ firstRow: 5, endRow: 6 });
    expect(shownRows({ firstRow: 1, endRow: 10 }, hidden)).toEqual({ firstRow: 1, endRow: 6 });
    expect(shownRows({ firstRow: 3, endRow: 6 }, hidden)).toEqual({ firstRow: 2, endRow: 3 });
    expect(shownRows({ firstRow: 2, endRow: 5 }, hidden)).toBeNull();
    expect(shownRows({ firstRow: 3, endRow: 4 }, [])).toEqual({ firstRow: 3, endRow: 4 });
  });
});

const SHEET = {
  sheet: 0,
  sheets: [
    { id: 0, name: "Sheet1" },
    { id: 7, name: "My sheet" },
  ],
  rows: 1000,
  totalRows: 1000,
  cols: 26,
} as unknown as SheetInfo;

function editor(
  sheet: SheetInfo | null,
  names: readonly Partial<WireNamedRangeInfo>[] = [],
  other?: SheetInfo,
) {
  const done: string[] = [];
  const span = (s: Span) => `${s.firstRow}-${s.endRow}:${s.firstCol}-${s.endCol}`;
  const grid = {
    sheetSize: { rows: 1000, cols: 26 },
    selection: { row: 0, col: 0 },
    selectRange: (from: unknown, to: unknown) => done.push(JSON.stringify([from, to])),
    selectRows: (first: number, last: number) => done.push(`rows ${first}-${last}`),
    selectCols: (first: number, last: number) => done.push(`cols ${first}-${last}`),
    addSelectionRange: (s: Span) => done.push(`and ${span(s)}`),
    reveal: () => {},
  } as unknown as Grid;
  let listener: ((message: EngineMessage) => boolean) | null = null;
  const handle = {
    state: () => ({ sheet }),
    sheetId: () => sheet?.sheet ?? 0,
    intercept: (one: (message: EngineMessage) => boolean) => {
      listener = one;
      return () => {
        listener = null;
      };
    },
    send: () => {
      listener?.({ kind: "namedRangesListed", items: names as WireNamedRangeInfo[] });
      return true;
    },
    onSheet: (id: number, then: (grid: Grid, arrived?: SheetInfo) => void) => {
      done.push(`sheet ${id}`);
      then(grid, other);
    },
  } as unknown as EditorHandle;
  return { handle, done };
}

describe("going to a text", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  const go = async (text: string, names?: readonly Partial<WireNamedRangeInfo>[]) => {
    const { handle, done } = editor(SHEET, names);
    return { problem: await goTo(handle, text), done };
  };

  it("selects what it refers to, on the sheet it is on", async () => {
    expect(await go("B200")).toEqual({
      problem: null,
      done: ["sheet 0", '[{"row":199,"col":1},{"row":199,"col":1}]'],
    });
    expect((await go("5:6")).done).toEqual(["sheet 0", "rows 4-5"]);
    expect((await go("B:C")).done).toEqual(["sheet 0", "cols 1-2"]);
    expect((await go("A1,5:5,B:B,D4")).done.slice(2)).toEqual([
      "and 4-5:0-26",
      "and 0-1000:1-2",
      "and 3-4:3-4",
    ]);
    expect((await go("'MY SHEET'!C3")).done).toEqual([
      "sheet 7",
      '[{"row":2,"col":2},{"row":2,"col":2}]',
    ]);
  });

  it("knows the names that stand for cells", async () => {
    const names = [
      { name: "Rate", scope: undefined, formula: "=Sheet1!$V$1" },
      { name: "rate", scope: 0, formula: "='My sheet'!$A$2:$B$3" },
      { name: "Third", scope: undefined, formula: "=0.25" },
      { name: "Elsewhere", scope: 7, formula: "=Sheet1!$A$1" },
    ];
    // The sheet's own comes before the workbook's.
    expect((await go("RATE", names)).done).toEqual([
      "sheet 7",
      '[{"row":1,"col":0},{"row":2,"col":1}]',
    ]);
    expect(await go("third", names)).toEqual({
      problem: "“third” does not stand for cells.",
      done: [],
    });
    expect((await go("Elsewhere", names)).problem).toBe(
      "“Elsewhere” is neither a reference nor a name.",
    );
  });

  it("says what is wrong", async () => {
    expect((await go("nothing")).problem).toBe("“nothing” is neither a reference nor a name.");
    expect((await go("Sheet9!A1")).problem).toBe("There is no worksheet “Sheet9”.");
    expect((await go("A1001")).problem).toBe("That is outside the worksheet.");
    expect((await go("AA1")).problem).toBe("That is outside the worksheet.");
    const { handle } = editor(null);
    expect(await goTo(handle, "A1")).toBe("There is no worksheet to go to.");
  });

  it("counts the rows a filter hides, and says that they are hidden", async () => {
    const filter = { hiddenRowSpans: [[2, 4]] };
    const sheet = { ...SHEET, filter } as unknown as SheetInfo;
    const { handle, done } = editor(sheet);
    expect(await goTo(handle, "A7")).toBeNull();
    expect(done).toEqual(["sheet 0", '[{"row":3,"col":0},{"row":3,"col":0}]']);
    expect(await goTo(handle, "A4")).toBe("Those rows are hidden by the filter.");
    expect(await goTo(handle, "B:B")).toBeNull();
  });

  it("goes by the size and the filter of the sheet it goes to", async () => {
    const there = {
      ...SHEET,
      sheet: 7,
      rows: 17,
      totalRows: 20,
      cols: 4,
      filter: { hiddenRowSpans: [[2, 4]] },
    } as unknown as SheetInfo;
    const go = async (text: string) => {
      const { handle, done } = editor(SHEET, [], there);
      return { problem: await goTo(handle, text), done };
    };
    expect(await go("'My sheet'!A7")).toEqual({
      problem: null,
      done: ["sheet 7", '[{"row":3,"col":0},{"row":3,"col":0}]'],
    });
    expect((await go("'My sheet'!A4")).problem).toBe("Those rows are hidden by the filter.");
    expect((await go("'My sheet'!A21")).problem).toBe("That is outside the worksheet.");
    expect((await go("'My sheet'!E1")).problem).toBe("That is outside the worksheet.");
    // This one is as large as it is, and what is hidden on the other is none of its business.
    const here = editor({ ...SHEET, filter: there.filter } as unknown as SheetInfo, [], {
      ...SHEET,
      sheet: 7,
    } as SheetInfo);
    expect(await goTo(here.handle, "'My sheet'!A4")).toBeNull();
    expect(here.done[1]).toBe('[{"row":3,"col":0},{"row":3,"col":0}]');
  });

  it("knows a name that is like a cell there is none of", async () => {
    const names = [{ name: "XFE1", scope: undefined, formula: "=Sheet1!$B$2" }];
    expect((await go("xfe1", names)).done).toEqual([
      "sheet 0",
      '[{"row":1,"col":1},{"row":1,"col":1}]',
    ]);
    expect((await go("XFE1")).problem).toBe("That is outside the worksheet.");
  });

  it("does not wait for an engine that does not answer", async () => {
    vi.useFakeTimers();
    const { handle } = editor(SHEET);
    const silent = { ...handle, send: () => true } as EditorHandle;
    const answer = goTo(silent, "Rate");
    await vi.advanceTimersByTimeAsync(3_000);
    expect(await answer).toBe("“Rate” is neither a reference nor a name.");
  });
});
