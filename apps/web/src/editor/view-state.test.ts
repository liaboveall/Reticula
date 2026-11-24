import type { GridView } from "@reticula/grid";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { parseView, ViewMemory, ViewStore } from "./view-state.ts";

const KEY = "reticula:view:v1";

const VIEW: GridView = {
  zoom: 0.75,
  row: 500_000,
  rowFraction: 0.25,
  col: 3,
  colFraction: 0,
  selection: { anchor: { row: 500_004, col: 5 }, focus: { row: 500_010, col: 6 }, scope: "cells" },
};

const viewOf = (row: number): GridView => ({ ...VIEW, row });

/** A storage with room for so many characters. */
class FakeStorage implements Pick<Storage, "getItem" | "setItem"> {
  readonly items = new Map<string, string>();
  room = Number.POSITIVE_INFINITY;
  writes = 0;

  getItem(key: string): string | null {
    return this.items.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    if (value.length > this.room) {
      throw new DOMException("full", "QuotaExceededError");
    }
    this.writes++;
    this.items.set(key, value);
  }
}

let storage: FakeStorage;
let store: ViewStore;

const documents = () =>
  (JSON.parse(storage.getItem(KEY) ?? "{}").documents as { id: string }[]).map(({ id }) => id);

beforeEach(() => {
  storage = new FakeStorage();
  store = new ViewStore(() => storage as unknown as Storage);
});

describe("views that are stored", () => {
  it("are found by their document and sheet", () => {
    store.write("a", 0, VIEW);
    store.write("a", 7, viewOf(1));
    store.write("b", 0, viewOf(2));
    expect(store.read("a")).toEqual(
      new Map([
        [0, VIEW],
        [7, viewOf(1)],
      ]),
    );
    expect(store.read("b")).toEqual(new Map([[0, viewOf(2)]]));
    expect(store.read("c").size).toBe(0);
  });

  it("are those of the fifty documents that were open last", () => {
    for (let index = 0; index < 60; index++) {
      store.write(`d${index}`, 0, viewOf(index));
    }
    store.write("d10", 1, viewOf(99));
    expect(documents()).toHaveLength(50);
    expect(documents()[0]).toBe("d11");
    expect(documents().at(-1)).toBe("d10");
    expect(store.read("d10").size).toBe(2);
    expect(store.read("d9").size).toBe(0);
  });

  it("do not take those of sheets that another tab has written away", () => {
    const other = new ViewStore(() => storage as unknown as Storage);
    store.write("a", 0, VIEW);
    other.write("a", 1, viewOf(1));
    store.write("a", 0, viewOf(2));
    expect([...store.read("a").keys()]).toEqual([0, 1]);
  });

  it("make room for the newest one when the storage is full", () => {
    for (let index = 0; index < 20; index++) {
      store.write(`d${index}`, 0, viewOf(index));
    }
    storage.room = 600;
    store.write("new", 0, VIEW);
    expect(documents()).toEqual(["new"]);
    storage.room = 10;
    expect(() => store.write("newer", 0, VIEW)).not.toThrow();
    expect(documents()).toEqual(["new"]);
  });

  it("may be anything, and the storage may be forbidden", () => {
    for (const text of [
      "",
      "{",
      "null",
      "[]",
      '{"documents":7}',
      '{"documents":[null,{"id":1}]}',
    ]) {
      storage.items.set(KEY, text);
      expect(store.read("a").size).toBe(0);
      store.write("a", 0, VIEW);
      expect(store.read("a").get(0)).toEqual(VIEW);
    }
    const forbidden = new ViewStore(() => {
      throw new DOMException("no", "SecurityError");
    });
    expect(forbidden.read("a").size).toBe(0);
    expect(() => forbidden.write("a", 0, VIEW)).not.toThrow();
  });

  it("are taken only as they were written", () => {
    expect(parseView(JSON.parse(JSON.stringify(VIEW)))).toEqual(VIEW);
    expect(parseView({ ...VIEW, selection: { anchor: { row: 1 } } })).toEqual({
      ...VIEW,
      selection: null,
    });
    expect(
      parseView({ ...VIEW, selection: { ...VIEW.selection, scope: "planes" } })?.selection?.scope,
    ).toBe("cells");
    for (const wrong of [
      { zoom: 3 },
      { zoom: "1" },
      { zoom: null },
      { row: -1 },
      { row: 1.5 },
      { col: 1e300 },
      { rowFraction: 2 },
      { colFraction: Number.NaN },
    ]) {
      expect(parseView({ ...VIEW, ...wrong })).toBeNull();
    }
    for (const wrong of [null, 7, "view", [], {}]) {
      expect(parseView(wrong)).toBeNull();
    }
  });
});

describe("what is remembered of the sheets", () => {
  let shown: GridView;
  const grid = { view: () => shown };

  beforeEach(() => {
    vi.useFakeTimers();
    shown = viewOf(0);
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("is where each of them was left", () => {
    const memory = new ViewMemory(grid, store);
    expect(memory.open("a", 0, false)).toEqual({ zoom: 1 });
    shown = viewOf(10);
    expect(memory.open("a", 1, false)).toEqual({ zoom: 1 });
    shown = viewOf(20);
    expect(memory.open("a", 0, false)).toEqual(viewOf(10));
    expect(memory.open("b", 0, false)).toEqual({ zoom: 1 });
    expect(new ViewMemory(grid, store).open("a", 1, false)).toEqual(viewOf(20));
  });

  it("is written down when it has not changed for a while, and not over again", () => {
    const memory = new ViewMemory(grid, store);
    memory.touch();
    vi.advanceTimersByTime(10_000);
    expect(storage.writes).toBe(0);
    memory.open("a", 0, false);
    for (let frame = 0; frame < 20; frame++) {
      shown = viewOf(frame);
      memory.touch();
      vi.advanceTimersByTime(frame < 19 ? 100 : 0);
    }
    vi.advanceTimersByTime(499);
    expect(storage.writes).toBe(0);
    vi.advanceTimersByTime(1);
    expect(store.read("a").get(0)).toEqual(viewOf(19));
    memory.touch();
    vi.advanceTimersByTime(500);
    expect(storage.writes).toBe(1);
    shown = viewOf(30);
    memory.save();
    expect(store.read("a").get(0)).toEqual(viewOf(30));
  });

  it("is not where one looked in a past version or a draft", () => {
    const memory = new ViewMemory(grid, store);
    memory.open("a", 0, false);
    shown = viewOf(10);
    // It starts from where the document was left, and keeps its own place from there.
    expect(memory.open("a", 0, true)).toEqual(viewOf(10));
    const { writes } = storage;
    shown = viewOf(20);
    memory.touch();
    vi.advanceTimersByTime(1_000);
    expect(memory.open("a", 0, true)).toEqual(viewOf(20));
    expect(memory.open("a", 3, true)).toEqual({ zoom: 1 });
    shown = viewOf(40);
    expect(memory.open("a", 0, false)).toEqual(viewOf(10));
    expect(storage.writes).toBe(writes);
    expect(store.read("a").has(3)).toBe(false);
    // The next preview begins where the document is again.
    shown = viewOf(11);
    expect(memory.open("a", 0, true)).toEqual(viewOf(11));
  });
});
