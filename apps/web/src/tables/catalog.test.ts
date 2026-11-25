import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  checksum,
  hasLeftovers,
  newest,
  parseSlot,
  readCatalog,
  type TableEntry,
} from "./catalog.ts";
import { FakeDirectory } from "./fake-storage.ts";

const entry: TableEntry = { id: "t1", name: "订单", rows: 10, cols: 3, bytes: 4_096 };

function slot(generation: number, tables: readonly TableEntry[]): string {
  const json = JSON.stringify({ generation, tables });
  return `${checksum(json)}\n${json}`;
}

let root: FakeDirectory;

beforeEach(() => {
  root = new FakeDirectory();
  vi.stubGlobal("navigator", { storage: { getDirectory: async () => root } });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

async function docDir(): Promise<FakeDirectory> {
  const top = await root.getDirectoryHandle("reticula-v1", { create: true });
  const tables = await top.getDirectoryHandle("tables", { create: true });
  return tables.getDirectoryHandle("doc", { create: true });
}

describe("a slot of the catalogue", () => {
  it("is read when it is whole", () => {
    expect(parseSlot(slot(3, [entry]))).toEqual({ generation: 3, tables: [entry] });
  });

  it("is not when a part of it is missing or another", () => {
    const whole = slot(3, [entry]);
    for (let length = 0; length < whole.length; length++) {
      expect(parseSlot(whole.slice(0, length))).toBeNull();
    }
    expect(parseSlot(whole.replace("t1", "t2"))).toBeNull();
    expect(parseSlot(`${whole}x`)).toBeNull();
    expect(parseSlot("")).toBeNull();
  });

  it("counts by its generation", () => {
    const older = { generation: 1, tables: [] };
    const newer = { generation: 2, tables: [entry] };
    expect(newest([older, newer])).toBe(newer);
    expect(newest([newer, older])).toBe(newer);
    expect(newest([null, older])).toBe(older);
    expect(newest([null, null])).toBeNull();
  });
});

describe("the tables of a workbook", () => {
  it("are none where there is no directory, or no file system", async () => {
    expect(await readCatalog("doc")).toEqual([]);
    vi.stubGlobal("navigator", {});
    expect(await readCatalog("doc")).toEqual([]);
  });

  it("are those of the newer slot that is whole", async () => {
    const dir = await docDir();
    dir.put("catalog-a", slot(4, [entry]));
    expect(await readCatalog("doc")).toEqual([entry]);
    dir.put("catalog-b", slot(5, []));
    expect(await readCatalog("doc")).toEqual([]);
    dir.put("catalog-b", slot(5, []).slice(0, 12));
    expect(await readCatalog("doc")).toEqual([entry]);
  });

  it("are not known when neither slot is whole", async () => {
    const dir = await docDir();
    dir.put("catalog-a", "1\n{}");
    expect(await readCatalog("doc")).toBeNull();
  });
});

describe("what is left of imports that were cut short", () => {
  it("is told from tables that are listed", async () => {
    expect(await hasLeftovers("doc")).toBe(false);
    const dir = await docDir();
    dir.put("t1.rtbl", "x");
    // The first one, of which there is no catalogue yet that has any tables.
    expect(await hasLeftovers("doc")).toBe(true);
    dir.put("catalog-a", slot(1, [entry]));
    expect(await hasLeftovers("doc")).toBe(false);
    dir.put("t2.rtbl", "x");
    expect(await hasLeftovers("doc")).toBe(true);
  });
});
