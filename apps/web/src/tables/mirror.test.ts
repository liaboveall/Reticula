import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeDirectory } from "./fake-storage.ts";
import { makeRoom, openMirror } from "./mirror.ts";

let root: FakeDirectory;

const copies = () => root.at("reticula-v1", "tables-remote", "user");
const names = () => [...copies().files.keys()].sort();

beforeEach(() => {
  root = new FakeDirectory();
  FakeDirectory.room = Number.POSITIVE_INFINITY;
  vi.stubGlobal("navigator", { storage: { getDirectory: async () => root } });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("a copy of a table", () => {
  it("is where it was when it is opened again, until it is removed", async () => {
    const first = await openMirror("user", "sum", 10);
    first?.file.append(Uint8Array.of(1, 2, 3));
    first?.file.close();
    const second = await openMirror("user", "sum", 10);
    expect(second?.file.size).toBe(3);
    await second?.remove();
    expect(names()).toEqual([]);
  });

  it("is not there for a second tab", async () => {
    await openMirror("user", "sum", 10);
    expect(await openMirror("user", "sum", 10)).toBeNull();
    expect(await openMirror("user", "other", 10)).not.toBeNull();
  });

  it("is not there where files cannot be kept", async () => {
    vi.stubGlobal("navigator", {});
    expect(await openMirror("user", "sum", 10)).toBeNull();
  });
});

describe("room for copies", () => {
  const dir = () => copies() as unknown as FileSystemDirectoryHandle;

  beforeEach(() => {
    for (const name of ["a", "b", "c"]) {
      copies().putBytes(name, new Uint8Array(400));
    }
    copies().put("doc.json", "[]");
  });

  it("is made where the ones used longest ago are", async () => {
    await makeRoom(dir(), "new", 300, 1_200);
    expect(names()).toEqual(["b", "c", "doc.json"]);
    await makeRoom(dir(), "new", 900, 1_000);
    expect(names()).toEqual(["doc.json"]);
  });

  it("counts what a copy already has once", async () => {
    await makeRoom(dir(), "a", 400, 1_200);
    expect(names()).toEqual(["a", "b", "c", "doc.json"]);
  });

  it("is not made where a table is open", async () => {
    await (await copies().getFileHandle("a")).createSyncAccessHandle();
    await makeRoom(dir(), "new", 300, 1_000);
    expect(names()).toEqual(["a", "doc.json"]);
  });

  it("goes by when a copy was opened last, not only by what came", async () => {
    (await openMirror("user", "a", 400))?.file.close();
    await makeRoom(dir(), "new", 300, 800);
    expect(names()).toEqual(["a", "doc.json"]);
  });
});
