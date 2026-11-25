import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HeldCommits, HOLD_MS } from "./held-commits.ts";

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

function host() {
  const rowIds = new Map<number, string>();
  const colIds = new Map<number, string>();
  const log: string[] = [];
  const state = { sheet: 0, readOnly: false, sheetSize: { rows: 100, cols: 20 } };
  const held = new HeldCommits({
    grid: {
      rowIdAt: (row) => rowIds.get(row) ?? null,
      colIdAt: (col) => colIds.get(col) ?? null,
      get sheetSize() {
        return state.sheetSize;
      },
    },
    sheetId: () => state.sheet,
    inspect: (sheet, { rowId, colId }) => log.push(`${sheet}/${rowId}/${colId}?`),
    commit: ({ sheet, raw }, { rowId, colId }) => {
      if (!state.readOnly) {
        log.push(`${sheet}/${rowId}/${colId}=${raw}`);
      }
      return !state.readOnly;
    },
    fail: ({ row, col, raw }) => log.push(`failed ${row}:${col} ${raw}`),
  });
  return { rowIds, colIds, log, state, held };
}

describe("what was typed before its cell was known", () => {
  it("is committed at once if the cell is known", () => {
    const { rowIds, colIds, log, held } = host();
    rowIds.set(5, "r5");
    colIds.set(1, "c1");
    held.hold({ sheet: 0, row: 5, col: 1, raw: "a" });
    expect(log).toEqual(["0/r5/c1=a"]);
    held.clear();
    vi.advanceTimersByTime(HOLD_MS);
    expect(log).toHaveLength(1);
  });

  it("is committed once the page with the cell has come, in the order it was typed", () => {
    const { rowIds, colIds, log, held } = host();
    held.hold({ sheet: 0, row: 5, col: 1, raw: "a" });
    held.hold({ sheet: 0, row: 6, col: 1, raw: "b" });
    held.hold({ sheet: 0, row: 5, col: 1, raw: "c" });
    held.retry();
    expect(log).toEqual([]);
    rowIds.set(5, "r5");
    held.retry();
    expect(log).toEqual([]);
    colIds.set(1, "c1");
    held.retry();
    expect(log).toEqual(["0/r5/c1=a", "0/r5/c1=c"]);
    rowIds.set(6, "r6");
    held.retry();
    held.retry();
    expect(log.slice(2)).toEqual(["0/r6/c1=b"]);
    vi.advanceTimersByTime(HOLD_MS * 2);
    expect(log).toHaveLength(3);
  });

  it("waits for its own sheet to be in view", () => {
    const { rowIds, colIds, log, state, held } = host();
    held.hold({ sheet: 0, row: 0, col: 0, raw: "x" });
    state.sheet = 1;
    state.sheetSize = { rows: 7, cols: 7 };
    rowIds.set(0, "other");
    colIds.set(0, "other");
    held.retry();
    expect(log).toEqual([]);
    state.sheet = 0;
    state.sheetSize = { rows: 100, cols: 20 };
    rowIds.set(0, "r");
    colIds.set(0, "c");
    held.retry();
    expect(log).toEqual(["0/r/c=x"]);
  });

  it("is for the row and the column it was typed into, if one of them was known", () => {
    const { rowIds, colIds, log, state, held } = host();
    colIds.set(1, "c1");
    held.hold({ sheet: 0, row: 5, col: 1, raw: "a" });
    // Somebody inserts a column on the left of it.
    state.sheetSize = { rows: 100, cols: 21 };
    colIds.set(1, "new");
    colIds.set(2, "c1");
    rowIds.set(5, "r5");
    held.retry();
    expect(log).toEqual(["0/r5/c1=a"]);

    held.hold({ sheet: 0, row: 8, col: 1, raw: "b" });
    state.sheetSize = { rows: 101, cols: 21 };
    rowIds.set(8, "r7");
    held.retry();
    expect(log.slice(1)).toEqual(["failed 8:1 b"]);
  });

  it("is not entered into another cell that has come into its place", () => {
    for (const sheetSize of [
      { rows: 101, cols: 20 },
      { rows: 99, cols: 20 },
      { rows: 100, cols: 21 },
    ]) {
      const { rowIds, colIds, log, state, held } = host();
      held.hold({ sheet: 0, row: 5, col: 1, raw: "a" });
      state.sheetSize = sheetSize;
      rowIds.set(5, "r4");
      colIds.set(1, "c1");
      held.retry();
      expect(log).toEqual(["failed 5:1 a"]);
      vi.advanceTimersByTime(HOLD_MS);
      expect(log).toHaveLength(1);
    }
  });

  it("goes behind what is in the cell, once that is known", () => {
    const { rowIds, colIds, log, held } = host();
    const answer = (rowId: string, raw: string, sheet = 0) =>
      held.retry({ sheet, rowId, colId: "c1", raw });
    const rest = (row: number, raw: string) =>
      held.hold({ sheet: 0, row, col: 1, raw, isRest: true });
    colIds.set(1, "c1");
    rest(5, "+1");
    expect(log).toEqual([]);
    // It is asked for once it is known which cell it is.
    rowIds.set(5, "r5").set(6, "r6");
    held.retry();
    expect(log.splice(0)).toEqual(["0/r5/c1?"]);
    answer("r5", "wrong", 1);
    answer("r4", "wrong");
    expect(new Set(log.splice(0))).toEqual(new Set(["0/r5/c1?"]));
    // What has been added to it meanwhile goes behind that.
    rest(5, "+2");
    log.length = 0;
    answer("r5", "=A1");
    expect(log.splice(0)).toEqual(["0/r5/c1==A1+1", "0/r5/c1==A1+1+2"]);
    answer("r5", "=A1");
    expect(log).toEqual([]);
    rest(6, "!");
    expect(log.splice(0)).toEqual(["0/r6/c1?"]);
    vi.advanceTimersByTime(HOLD_MS);
    expect(log.splice(0)).toEqual(["failed 6:1 !"]);

    // Which cell it is, is known to whoever has typed into it.
    rowIds.clear();
    const commit = { sheet: 0, row: 9, col: 1, raw: "+3", isRest: true };
    held.hold(commit, { rowId: "r9", colId: "c1" });
    answer("r9", "x");
    expect(log).toEqual(["0/r9/c1?", "0/r9/c1=x+3"]);
  });

  it("is not lost without a word if nothing may be changed by then", () => {
    const { rowIds, colIds, log, state, held } = host();
    held.hold({ sheet: 0, row: 5, col: 1, raw: "a" });
    state.readOnly = true;
    rowIds.set(5, "r5");
    colIds.set(1, "c1");
    held.retry();
    expect(log).toEqual(["failed 5:1 a"]);
    state.readOnly = false;
    held.retry();
    vi.advanceTimersByTime(HOLD_MS);
    expect(log).toHaveLength(1);
  });

  it("is given up after a while, or when another workbook is opened", () => {
    const { log, held } = host();
    held.hold({ sheet: 0, row: 1, col: 1, raw: "late" });
    vi.advanceTimersByTime(HOLD_MS - 1);
    held.hold({ sheet: 0, row: 2, col: 2, raw: "later" });
    expect(log).toEqual([]);
    vi.advanceTimersByTime(1);
    expect(log).toEqual(["failed 1:1 late"]);
    held.clear();
    vi.advanceTimersByTime(HOLD_MS);
    expect(log).toEqual(["failed 1:1 late", "failed 2:2 later"]);
  });
});
