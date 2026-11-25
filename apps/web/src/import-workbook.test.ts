import { describe, expect, test, vi } from "vitest";
import type { EditorHandle } from "./commands/handle.ts";
import {
  fileKind,
  hasManyRows,
  importWorkbook,
  MAX_CSV_RAW_BYTES,
  titleOf,
} from "./import-workbook.ts";

describe("a file", () => {
  test.each([
    ["a.rtcl", "workbook", "a"],
    ["a.b.XLSX", "workbook", "a.b"],
    ["orders.csv", "text", "orders"],
    ["orders.TSV", "text", "orders"],
    ["notes.txt", "text", "notes"],
    ["big.parquet", "table", "big"],
    ["big.rtbl", "table", "big"],
    ["photo.png", null, "photo.png"],
    [".csv", "text", "Imported"],
    ["csv", null, "csv"],
  ])("%s is of a kind, and has a title", (name, kind, title) => {
    expect(fileKind(name)).toBe(kind);
    expect(titleOf(name)).toBe(title);
  });

  const rows = (count: number, length: number) =>
    new Blob([`${"x".repeat(length - 1)}\n`.repeat(count)]);

  test("of text has many rows if its first ones say so", async () => {
    expect(await hasManyRows(rows(1_000, 10))).toBe(false);
    expect(await hasManyRows(rows(990_000, 16))).toBe(false);
    expect(await hasManyRows(rows(1_010_000, 16))).toBe(true);
    // One long line is one row.
    expect(await hasManyRows(new Blob(["x".repeat(4 << 20)]))).toBe(false);
  });

  test("that is too large for a workbook has many rows, whatever is in it", async () => {
    const file = { size: MAX_CSV_RAW_BYTES + 1 } as Blob;
    expect(await hasManyRows(file)).toBe(true);
  });
});

describe("a file that becomes a workbook", () => {
  const importing = async (isCloud: boolean, name = "orders.csv") => {
    const done: string[] = [];
    const handle = {
      collab: () => (isCloud ? {} : null),
      commitPending: () => done.push("commit"),
      noteChange: () => {},
      freshDocId: () => "fresh",
      sendHost: ({ kind }: { kind: string }) => done.push(kind),
    } as unknown as EditorHandle;
    const onRejected = vi.fn();
    await importWorkbook(handle, new File(["a,b\n1,2\n"], name), {
      onNotice: vi.fn(),
      onRejected,
      onOpening: () => done.push("opening"),
    });
    return { done, onRejected };
  };

  test("leaves what is being typed to the one that is open", async () => {
    expect((await importing(false)).done).toEqual(["commit", "opening", "importCsv"]);
    expect((await importing(false, "a.rtcl")).done).toEqual(["commit", "opening", "importRtcl"]);
  });

  test("does not while one on the server is open, and why is said", async () => {
    for (const name of ["orders.csv", "a.rtcl", "a.xlsx"]) {
      const { done, onRejected } = await importing(true, name);
      expect(done).toEqual([]);
      expect(onRejected.mock.calls[0]?.[0].say()).toContain("Open a local workbook first");
    }
  });
});
