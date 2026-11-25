import { beforeEach, describe, expect, test } from "vitest";
import { FRESH_LOOK, parseLook, type TableLook, TableViews } from "./table-views.ts";

let stored: Map<string, string>;
let room: number;

const storage = {
  getItem: (key: string) => stored.get(key) ?? null,
  setItem: (key: string, value: string) => {
    if (value.length > room) {
      throw new DOMException("full", "QuotaExceededError");
    }
    stored.set(key, value);
  },
} as unknown as Storage;

const views = () => new TableViews(() => storage);

const LOOK: TableLook = {
  query: {
    filters: [
      { column: 1, condition: { kind: "oneOf", keys: ["a", "b"], blank: true } },
      { column: 2, condition: { kind: "number", op: "between", a: "1", b: "2.5" } },
    ],
    order: [{ column: 3, descending: true }],
  },
  widths: [
    [0, 80],
    [4, 200],
  ],
  frozenCols: 2,
  view: {
    zoom: 1.25,
    row: 9_000_000,
    rowFraction: 0.5,
    col: 1,
    colFraction: 0,
    selection: { anchor: { row: 5, col: 1 }, focus: { row: 6, col: 1 }, scope: "cells" },
  },
  report: null,
  crashes: 1,
};

beforeEach(() => {
  stored = new Map();
  room = 1_000_000;
});

describe("how a table was left", () => {
  test("comes back", () => {
    views().write("doc", "t1", LOOK);
    views().write("doc", "t2", FRESH_LOOK);
    expect(views().read("doc", "t1")).toEqual(LOOK);
    expect(views().read("doc", "t2")).toEqual(FRESH_LOOK);
    expect(views().read("doc", "t3")).toEqual(FRESH_LOOK);
    expect(views().read("other", "t1")).toEqual(FRESH_LOOK);
    expect(views().read("doc", "constructor")).toEqual(FRESH_LOOK);
  });

  test("goes with the table", () => {
    views().write("doc", "t1", LOOK);
    views().write("doc", "t2", LOOK);
    views().setActive("doc", "t1");
    views().forget("doc", "t1");
    expect(views().read("doc", "t1")).toEqual(FRESH_LOOK);
    expect(views().read("doc", "t2")).toEqual(LOOK);
    expect(views().active("doc")).toBeNull();
  });

  test("is told whatever is in the storage", () => {
    for (const value of ["", "null", "[]", "{", '{"documents":3}', '{"documents":[{"id":1}]}']) {
      stored.set("reticula:table-view:v1", value);
      expect(views().read("doc", "t"), value).toEqual(FRESH_LOOK);
      expect(views().active("doc"), value).toBeNull();
    }
    expect(
      parseLook({
        query: {
          filters: [
            { column: -1, condition: { kind: "blank" } },
            { column: 1 },
            { column: 1, condition: { kind: "oneOf" } },
            { column: 1, condition: { kind: "blank" } },
          ],
          order: [{ column: 1.5, descending: true }, { column: 2, descending: false }, null],
        },
        widths: [[1], [1, -5], "x", [2, 90]],
        frozenCols: "3",
        view: { zoom: 99 },
        report: { columns: 1 },
        crashes: -1,
      }),
    ).toEqual({
      ...FRESH_LOOK,
      query: {
        filters: [{ column: 1, condition: { kind: "blank" } }],
        order: [{ column: 2, descending: false }],
      },
      widths: [[2, 90]],
    });
  });

  test("leaves out what would be too much", () => {
    const keys = Array.from({ length: 30_000 }, (_, n) => `value ${n}`);
    const filters = [{ column: 0, condition: { kind: "oneOf", keys, blank: false } as const }];
    views().write("doc", "t", { ...LOOK, query: { filters, order: [] } });
    expect(views().read("doc", "t")).toEqual({ ...LOOK, query: FRESH_LOOK.query });
  });

  test("makes room for the one that is in use", () => {
    for (let n = 0; n < 60; n++) {
      views().write(`doc ${n}`, "t", LOOK);
    }
    expect(views().read("doc 9", "t")).toEqual(FRESH_LOOK);
    expect(views().read("doc 10", "t")).toEqual(LOOK);
    room = 2_000;
    views().write("doc 60", "t", LOOK);
    expect(views().read("doc 60", "t")).toEqual(LOOK);
    expect(views().read("doc 59", "t")).toEqual(FRESH_LOOK);
    room = 0;
    expect(() => views().setActive("doc 60", "t")).not.toThrow();
  });
});
