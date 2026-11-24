import type { Grid, Selection, Span } from "@reticula/grid";
import type { EngineCommand, SheetInfo } from "@reticula/protocol";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { EngineMessage } from "../engine/client.ts";
import { KEYS } from "../keymap.ts";
import type { EditorHandle, EditorState, MutateOptions } from "./handle.ts";
import { COMMANDS, commandOf, isEnabled, run } from "./registry.ts";
import { findCommands } from "./search.ts";

const announced = vi.hoisted(() => [] as string[]);
vi.mock("../ui/announcer.ts", () => ({ announce: (text: string) => announced.push(text) }));
// What is loaded later may arrive when the test is over.
vi.mock("../i18n/zh-engine.ts", () => ({ default: {} }));
vi.mock("../i18n/zh-functions.ts", () => ({ default: {} }));

const SHEET = {
  sheet: 1,
  sheets: [
    { id: 0, name: "Sheet1" },
    { id: 1, name: "Sheet2" },
    { id: 2, name: "Sheet3" },
  ],
  rows: 100,
  totalRows: 100,
  cols: 20,
  canUndo: true,
  canRedo: false,
  frozenRows: 0,
  frozenCols: 0,
} as unknown as SheetInfo;

const span = (firstRow: number, endRow: number, firstCol: number, endCol: number): Span => ({
  firstRow,
  endRow,
  firstCol,
  endCol,
});

interface Fake {
  readonly state?: Partial<EditorState>;
  readonly range?: Span;
  /** The texts of cells, by "row:col". Others are empty. */
  readonly texts?: Readonly<Record<string, string | null>>;
}

function editor({ state = {}, range = span(2, 3, 1, 2), texts = {} }: Fake = {}) {
  const log: unknown[] = [];
  const listeners = new Set<(message: EngineMessage) => boolean>();
  const selection = {
    row: range.firstRow,
    col: range.firstCol,
    range,
    ranges: [range],
    rowId: "r",
    colId: "c",
  } as unknown as Selection;
  const full: EditorState = {
    sheet: SHEET,
    readOnly: false,
    selection,
    style: undefined,
    formatCode: "General",
    isCloud: false,
    ...state,
  };
  const grid = {
    zoom: 1,
    selection: { row: range.firstRow, col: range.firstCol },
    selectionRange: range,
    textAt: (row: number, col: number) => {
      const text = texts[`${row}:${col}`];
      return text === undefined ? "" : text;
    },
    rowIdAt: (row: number) => `r${row}`,
    colIdAt: (col: number) => `c${col}`,
    selectedFormatTarget: () => ({ scope: "cols", cols: [] }),
    selectRows: (first: number, last: number) => log.push(["rows", first, last]),
    selectCols: (first: number, last: number) => log.push(["cols", first, last]),
    moveSelectedRows: (direction: number) => {
      log.push(["move", direction]);
      return false;
    },
  } as unknown as Grid;
  const handle = {
    state: () => full,
    sheetId: () => full.sheet?.sheet ?? 0,
    askedSheetId: () => full.sheet?.sheet ?? 0,
    grid: () => grid,
    cellEditor: () => null,
    commitPending: () => log.push("commit"),
    mutate: (command: EngineCommand, options?: MutateOptions) => {
      log.push(options?.action ? [command, options.action] : command);
      return true;
    },
    send: (command: EngineCommand) => {
      log.push(command);
      return true;
    },
    fill: (src: Span, dst: Span) => {
      log.push({ src, dst });
      return true;
    },
    intercept: (listener: (message: EngineMessage) => boolean) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    switchSheet: (sheet: number) => log.push(["switch", sheet]),
    openDialog: (dialog: unknown) => log.push(dialog),
  } as unknown as EditorHandle;
  const say = (message: EngineMessage) => [...listeners].some((listener) => listener(message));
  return { handle, log, say, state: full };
}

/** What a command does, without that it accepts what is being typed first. */
function outcome(id: string, fake: Fake = {}, sheet?: number): unknown[] {
  const { handle, log } = editor(fake);
  run(handle, id, sheet);
  return log.filter((entry) => entry !== "commit");
}

beforeEach(() => {
  announced.length = 0;
});

describe("the commands", () => {
  it("have a name each, and one for every key", () => {
    expect(new Set(COMMANDS.map(({ id }) => id)).size).toBe(COMMANDS.length);
    for (const { id, title } of COMMANDS) {
      expect(title(), id).toMatch(/^\S.*\S$/);
    }
    const titles = COMMANDS.map(({ title }) => title());
    expect(titles.filter((title, at) => titles.indexOf(title) !== at)).toEqual([]);
    for (const [command] of KEYS) {
      expect(commandOf(command), command).toBeDefined();
    }
  });

  it("cover what there is to do", () => {
    const ids = COMMANDS.map(({ id }) =>
      id.replace(/^(numberFormat|textColor|fillColor|zoom):.*/, "$1"),
    );
    expect([...new Set(ids)]).toEqual([
      "undo",
      "redo",
      "cut",
      "copy",
      "paste",
      "clearContents",
      "fillDown",
      "fillRight",
      "fillSeriesDown",
      "fillSeriesUp",
      "fillSeriesRight",
      "fillSeriesLeft",
      "autoFillDown",
      "find",
      "replace",
      "goTo",
      "selectRows",
      "selectCols",
      "selectAll",
      "bold",
      "italic",
      "strikethrough",
      "alignLeft",
      "alignCenter",
      "alignRight",
      "numberFormat",
      "textColor",
      "fillColor",
      "borderThin",
      "borderMedium",
      "borderNone",
      "insertRowAbove",
      "insertRowBelow",
      "deleteRows",
      "insertColLeft",
      "insertColRight",
      "deleteCols",
      "moveRowsUp",
      "moveRowsDown",
      "moveColsLeft",
      "moveColsRight",
      "rowHeight",
      "colWidth",
      "autofitCols",
      "freeze:none",
      "freeze:row1",
      "freeze:col1",
      "freeze:row1col1",
      "freeze:active",
      "sort",
      "filter",
      "filterColumn",
      "clearFilters",
      "names",
      "resumeRecalc",
      "addSheet",
      "renameSheet",
      "moveSheetLeft",
      "moveSheetRight",
      "deleteSheet",
      "nextSheet",
      "previousSheet",
      "zoomIn",
      "zoomOut",
      "zoom",
      "rendererAuto",
      "rendererCanvas",
      "language:en",
      "language:zh-CN",
      "contextMenu",
      "nextRegion",
      "previousRegion",
      "workbooks",
      "newWorkbook",
      "import",
      "exportRtcl",
      "exportCsv",
      "exportXlsx",
      "cloud",
      "history",
      "ai",
      "palette",
      "keyboardHelp",
    ]);
  });

  it("change nothing of what may only be read, or is not there", () => {
    const reading = { readOnly: true };
    const changes = [
      "undo",
      "paste",
      "cut",
      "clearContents",
      "fillDown",
      "fillSeriesDown",
      "bold",
      "numberFormat:0%",
      "textColor:1",
      "borderThin",
      "insertRowAbove",
      "insertColRight",
      "deleteRows",
      "deleteCols",
      "moveRowsUp",
      "rowHeight",
      "autofitCols",
      "freeze:row1",
      "sort",
      "addSheet",
      "renameSheet",
      "deleteSheet",
    ];
    for (const id of changes) {
      expect(outcome(id, { state: reading, range: span(0, 3, 0, 3) }), id).toEqual([]);
      expect(outcome(id, { state: { sheet: null } }), id).toEqual([]);
    }
    expect(announced).toHaveLength(changes.length * 2);
    for (const id of ["copy", "find", "goTo", "zoomIn", "exportCsv", "nextSheet"]) {
      const command = commandOf(id);
      expect(command && isEnabled(command, editor({ state: reading }).state), id).toBe(true);
    }
  });

  it("accept what is being typed first, unless it has nothing to do with them", () => {
    for (const [id, expected] of [
      ["bold", true],
      ["palette", false],
      ["keyboardHelp", false],
      ["nextRegion", false],
    ] as const) {
      const { handle, log } = editor();
      vi.stubGlobal("document", { querySelectorAll: () => [], activeElement: null });
      run(handle, id);
      vi.unstubAllGlobals();
      expect(log.includes("commit"), id).toBe(expected);
    }
  });

  it("say so when they come to nothing", () => {
    expect(outcome("moveRowsUp")).toEqual([["move", -1]]);
    expect(announced).toEqual(["Not possible here: Move Rows Up"]);
    outcome("redo");
    expect(announced).toContain("Not possible here: Redo");
  });
});

describe("filling", () => {
  it("copies the first row or column over the rest, or the one before a single one", () => {
    const copied = (id: string, range: Span) => outcome(id, { range });
    const fill = (firstRow: number, rowCount: number, firstCol: number, colCount: number) => ({
      kind: "copyFill",
      sheet: 1,
      firstRow,
      rowCount,
      firstCol,
      colCount,
    });
    const down = (...at: [number, number, number, number]) => [
      [{ ...fill(...at), dir: "down" }, "filledRange"],
    ];
    const right = (...at: [number, number, number, number]) => [
      [{ ...fill(...at), dir: "right" }, "filledRange"],
    ];
    expect(copied("fillDown", span(2, 6, 1, 3))).toEqual(down(2, 4, 1, 2));
    expect(copied("fillDown", span(5, 6, 1, 3))).toEqual(down(4, 2, 1, 2));
    expect(copied("fillRight", span(2, 6, 1, 3))).toEqual(right(2, 4, 1, 2));
    expect(copied("fillRight", span(2, 6, 3, 4))).toEqual(right(2, 4, 2, 2));
    expect(copied("fillDown", span(0, 1, 1, 3))).toEqual([]);
    expect(copied("fillRight", span(2, 6, 0, 1))).toEqual([]);
  });

  it("continues what is at one side of the selection", () => {
    const range = span(1, 6, 2, 4);
    const texts = { "1:2": "1", "2:3": "x", "5:2": "9", "4:2": "8", "3:3": "y" };
    const source = (id: string, some: Fake) => (outcome(id, some)[0] as { src: Span }).src;
    // The second row counts, with something in its second column. The third is not empty either.
    expect(source("fillSeriesDown", { range, texts })).toEqual(span(1, 2, 2, 4));
    const gap = { ...texts, "3:3": "" };
    expect(source("fillSeriesDown", { range, texts: gap })).toEqual(span(1, 3, 2, 4));
    expect(source("fillSeriesUp", { range, texts: gap })).toEqual(span(4, 6, 2, 4));
    expect(source("fillSeriesRight", { range: span(1, 2, 0, 5), texts: { "1:0": "1" } })).toEqual(
      span(1, 2, 0, 1),
    );
    expect(
      source("fillSeriesLeft", { range: span(1, 2, 0, 5), texts: { "1:4": "1", "1:3": "2" } }),
    ).toEqual(span(1, 2, 3, 5));
    // What has not been loaded is not known to be anything.
    const unknown = { "0:0": "1", "1:0": "2", "2:0": null };
    expect(source("fillSeriesDown", { range: span(0, 9, 0, 1), texts: unknown })).toEqual(
      span(0, 2, 0, 1),
    );
    expect(outcome("fillSeriesDown", { range: span(0, 1, 0, 3) })).toEqual([]);
    expect(outcome("fillSeriesRight", { range: span(0, 3, 0, 1) })).toEqual([]);
  });
});

describe("rows and columns", () => {
  it("are inserted on the side that is asked for", () => {
    const range = span(3, 5, 1, 4);
    const inserted = (id: string, some = range) =>
      (outcome(id, { range: some })[0] as unknown[])[0];
    expect(inserted("insertRowAbove")).toMatchObject({ kind: "insertRows", afterRowId: "r2" });
    expect(inserted("insertRowBelow")).toMatchObject({ kind: "insertRows", afterRowId: "r4" });
    expect(inserted("insertColLeft")).toMatchObject({ kind: "insertCols", afterColId: "c0" });
    expect(inserted("insertColRight")).toMatchObject({ kind: "insertCols", afterColId: "c3" });
    const first = span(0, 1, 0, 1);
    expect(inserted("insertRowAbove", first)).toMatchObject({ afterRowId: null });
    expect(inserted("insertColLeft", first)).toMatchObject({ afterColId: null });
  });

  it("are frozen up to the active cell", () => {
    const frozen = (id: string, range = span(3, 4, 2, 3)) =>
      (outcome(id, { range })[0] as unknown[])[0];
    expect(frozen("freeze:active")).toMatchObject({ frozenRows: 3, frozenCols: 2 });
    expect(frozen("freeze:active", span(90, 91, 0, 1))).toMatchObject({ frozenRows: 32 });
    expect(frozen("freeze:active", span(0, 1, 0, 1))).toMatchObject({
      frozenRows: 1,
      frozenCols: 0,
    });
    expect(frozen("freeze:row1col1")).toMatchObject({ frozenRows: 1, frozenCols: 1 });
    expect(frozen("freeze:none")).toMatchObject({ frozenRows: 0, frozenCols: 0 });
  });
});

describe("worksheets", () => {
  it("are moved and gone to, as far as there are any", () => {
    const moved = (id: string, sheet?: number) => outcome(id, {}, sheet)[0];
    const move = (sheet: number, afterSheet: number | undefined) => [
      { kind: "moveSheet", sheet, afterSheet },
      "reorderedSheets",
    ];
    expect(moved("moveSheetLeft")).toEqual(move(1, undefined));
    expect(moved("moveSheetRight")).toEqual(move(1, 2));
    expect(moved("moveSheetLeft", 2)).toEqual(move(2, 0));
    expect(moved("moveSheetRight", 2)).toBeUndefined();
    expect(moved("moveSheetLeft", 0)).toBeUndefined();
    expect(outcome("nextSheet")).toEqual([["switch", 2]]);
    expect(outcome("previousSheet")).toEqual([["switch", 0]]);
    const last = { ...SHEET, sheet: 2 } as SheetInfo;
    expect(outcome("nextSheet", { state: { sheet: last } })).toEqual([]);
    expect(announced).toContain("Not possible here: Next Sheet");
    // From the one that is about to come into view, where the one in view is the first.
    const first = { ...SHEET, sheet: 0 } as SheetInfo;
    const { handle, log } = editor({ state: { sheet: first } });
    run({ ...handle, askedSheetId: () => 1 }, "previousSheet");
    expect(log).toContainEqual(["switch", 0]);
  });

  describe("that are deleted", () => {
    afterEach(() => {
      vi.useRealTimers();
    });

    const deleted = [{ kind: "deleteSheet", sheet: 1 }, "deletedSheet"];
    const asked = (id: number) => ({
      kind: "deleteSheet",
      tab: SHEET.sheets.find((tab) => tab.id === id),
    });

    async function deleting(fake: Fake, at: { row: number; col: number } | null, sheet?: number) {
      vi.useFakeTimers();
      const { handle, log, say } = editor(fake);
      run(handle, "deleteSheet", sheet);
      if (at) {
        say({ kind: "edgeJumped", sheet: sheet ?? 1, ...at, select: false });
      }
      await vi.advanceTimersByTimeAsync(2_000);
      return log.at(-1);
    }

    it("go at once if there is nothing on them", async () => {
      expect(await deleting({}, { row: 0, col: 0 })).toEqual(deleted);
    });

    it("are asked for if there is, or may be", async () => {
      expect(await deleting({ texts: { "0:0": "x" } }, null)).toEqual(asked(1));
      expect(await deleting({}, { row: 7, col: 0 })).toEqual(asked(1));
      // Of a sheet that is not in view, the first cell alone is not told from none.
      expect(await deleting({}, { row: 0, col: 0 }, 2)).toEqual(asked(2));
      expect(await deleting({}, null)).toEqual(asked(1));
    });

    it("are not the last one", () => {
      const only = { ...SHEET, sheets: [{ id: 1, name: "Sheet2" }] } as SheetInfo;
      expect(outcome("deleteSheet", { state: { sheet: only } })).toEqual([]);
    });
  });
});

describe("looking for a command", () => {
  const first = (query: string) => findCommands(query)[0]?.id;

  afterEach(async () => {
    const { initLocale } = await import("../i18n/index.ts");
    vi.stubGlobal("window", {
      location: { search: "?lang=en" },
      localStorage: { getItem: () => null },
      navigator: { languages: [] },
    });
    await initLocale("app");
    vi.unstubAllGlobals();
  });

  it("finds it by parts of its name, and by other words", () => {
    expect(findCommands("")).toHaveLength(COMMANDS.length);
    expect(first("bold")).toBe("bold");
    expect(first("row ab")).toBe("insertRowAbove");
    expect(first("column right")).toBe("insertColRight");
    expect(first("columns right")).toBe("moveColsRight");
    expect(first("colour red")).toBe("textColor:1");
    expect(first("webgpu")).toBe("rendererAuto");
    expect(first("chinese")).toBe("language:zh-CN");
    expect(first("english")).toBe("language:en");
    expect(findCommands("no such thing")).toEqual([]);
  });

  it("finds it by its Chinese and its English name in Chinese", async () => {
    vi.stubGlobal("document", { documentElement: {} });
    vi.stubGlobal("window", {
      location: { search: "?lang=zh" },
      localStorage: { getItem: () => null },
      navigator: { languages: [] },
    });
    const { initLocale, t } = await import("../i18n/index.ts");
    await initLocale("app");
    expect(t("Bold")).toBe("加粗");
    expect(first("加粗")).toBe("bold");
    expect(first("bold")).toBe("bold");
    expect(first("上方插入")).toBe("insertRowAbove");
    expect(first("insert row ab")).toBe("insertRowAbove");
    expect(t("Bold")).toBe("加粗");
  });
});
