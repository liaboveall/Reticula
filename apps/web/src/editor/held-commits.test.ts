import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { type CellIds, HeldCommits, HOLD_MS } from "./held-commits.ts";

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

function host() {
  const ids = new Map<string, CellIds>();
  const log: string[] = [];
  const state = { sheet: 0, readOnly: false };
  const held = new HeldCommits({
    idsOf: (row, col) => ids.get(`${row}:${col}`) ?? null,
    sheetId: () => state.sheet,
    commit: ({ sheet, raw }, { rowId, colId }) => {
      if (!state.readOnly) {
        log.push(`${sheet}/${rowId}/${colId}=${raw}`);
      }
      return !state.readOnly;
    },
    fail: ({ row, col, raw }) => log.push(`failed ${row}:${col} ${raw}`),
  });
  return { ids, log, state, held };
}

describe("what was typed before its cell was known", () => {
  it("is committed once the page with the cell has come, in the order it was typed", () => {
    const { ids, log, held } = host();
    held.hold({ sheet: 0, row: 5, col: 1, raw: "a" });
    held.hold({ sheet: 0, row: 6, col: 1, raw: "b" });
    held.hold({ sheet: 0, row: 5, col: 1, raw: "c" });
    held.retry();
    expect(log).toEqual([]);
    ids.set("5:1", { rowId: "r5", colId: "c1" });
    held.retry();
    expect(log).toEqual(["0/r5/c1=a", "0/r5/c1=c"]);
    ids.set("6:1", { rowId: "r6", colId: "c1" });
    held.retry();
    held.retry();
    expect(log.slice(2)).toEqual(["0/r6/c1=b"]);
    vi.advanceTimersByTime(HOLD_MS * 2);
    expect(log).toHaveLength(3);
  });

  it("waits for its own sheet to be in view", () => {
    const { ids, log, state, held } = host();
    ids.set("0:0", { rowId: "r", colId: "c" });
    state.sheet = 1;
    held.hold({ sheet: 2, row: 0, col: 0, raw: "x" });
    held.retry();
    expect(log).toEqual([]);
    state.sheet = 2;
    held.retry();
    expect(log).toEqual(["2/r/c=x"]);
  });

  it("is not lost without a word if nothing may be changed by then", () => {
    const { ids, log, state, held } = host();
    held.hold({ sheet: 0, row: 5, col: 1, raw: "a" });
    state.readOnly = true;
    ids.set("5:1", { rowId: "r5", colId: "c1" });
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
