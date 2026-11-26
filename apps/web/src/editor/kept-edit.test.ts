import { beforeEach, describe, expect, test, vi } from "vitest";
import { KEY, type KeptEdit, keepEdit, takeKeptEdit } from "./kept-edit.ts";

const edit: KeptEdit = {
  doc: "local:mine",
  sheet: 0,
  row: 3,
  col: 1,
  source: "=SUM(A1:A3",
  cursorUtf16: 10,
  inFormulaBar: true,
};

beforeEach(() => {
  const items = new Map<string, string>();
  vi.stubGlobal("sessionStorage", {
    getItem: (key: string) => items.get(key) ?? null,
    setItem: (key: string, value: string) => items.set(key, value),
    removeItem: (key: string) => items.delete(key),
  });
});

describe("what was being typed when the app was updated", () => {
  test("is kept by the name the editor looks for before loading this", () => {
    expect(KEY).toBe("reticula:kept-edit");
  });

  test("is had back once, in the workbook it was typed in", () => {
    keepEdit(edit, 1_000);
    expect(takeKeptEdit("local:mine", 2_000)).toEqual(edit);
    expect(takeKeptEdit("local:mine", 2_000)).toBeNull();
  });

  test("is not had in another workbook, and is gone then", () => {
    keepEdit(edit, 1_000);
    expect(takeKeptEdit("local:other", 2_000)).toBeNull();
    expect(takeKeptEdit("local:mine", 2_000)).toBeNull();
  });

  test("is not had long after", () => {
    keepEdit(edit, 1_000);
    expect(takeKeptEdit("local:mine", 1_000 + 6 * 60_000)).toBeNull();
  });

  test("keeps its caret within what it says", () => {
    keepEdit({ ...edit, cursorUtf16: 99 }, 1_000);
    expect(takeKeptEdit("local:mine", 1_000)?.cursorUtf16).toBe(edit.source.length);
  });

  test("is nothing when what is stored is not one", () => {
    sessionStorage.setItem("reticula:kept-edit", "{not json");
    expect(takeKeptEdit("local:mine")).toBeNull();
    sessionStorage.setItem("reticula:kept-edit", JSON.stringify({ doc: "local:mine" }));
    expect(takeKeptEdit("local:mine")).toBeNull();
  });

  test("is lost, as before, where there is no storage", () => {
    vi.stubGlobal("sessionStorage", {
      getItem: () => {
        throw new Error("denied");
      },
      setItem: () => {
        throw new Error("denied");
      },
    });
    expect(() => keepEdit(edit)).not.toThrow();
    expect(takeKeptEdit("local:mine")).toBeNull();
  });
});
