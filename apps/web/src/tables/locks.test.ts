import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { deleteTables, tablesLock } from "./delete.ts";
import { FakeChannel, FakeDirectory, FakeLocks } from "./fake-storage.ts";
import { TablesLock } from "./locks.ts";

let locks: FakeLocks;

beforeEach(() => {
  vi.useFakeTimers();
  locks = new FakeLocks();
  FakeChannel.all.clear();
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

interface Tab {
  readonly lock: TablesLock;
  readonly yields: boolean[];
}

function tab(tabId: string, hung = false, docId = "doc"): Tab {
  const yields: boolean[] = [];
  const channel = new FakeChannel();
  const lock = new TablesLock({
    docId,
    tabId,
    locks: locks as unknown as LockManager,
    channel: channel as unknown as BroadcastChannel,
    onYield: async (evicted) => {
      yields.push(evicted);
      if (hung && !evicted) {
        await new Promise(() => {});
      }
    },
  });
  return { lock, yields };
}

describe("the lock on the tables of a workbook", () => {
  it("is had by one", async () => {
    const [a, b] = [tab("a"), tab("b")];
    expect(await a.lock.acquire(false)).toBe(true);
    expect(await a.lock.acquire(false)).toBe(true);
    expect(await b.lock.acquire(false)).toBe(false);
    a.lock.release();
    await vi.advanceTimersByTimeAsync(0);
    expect(await b.lock.acquire(false)).toBe(true);
    expect(a.yields).toEqual([]);
  });

  it("is let go of for whoever asks", async () => {
    const [a, b] = [tab("a"), tab("b")];
    await a.lock.acquire(false);
    expect(await b.lock.acquire(true)).toBe(true);
    expect(a.yields).toEqual([false]);
    expect(a.lock.held).toBe(false);
    expect(b.yields).toEqual([]);
  });

  it("is let go of with the workbook, but not for the own tab or another workbook", async () => {
    const a = tab("a");
    const other = new FakeChannel();
    await a.lock.acquire(false);
    other.postMessage({ kind: "yield", docId: "doc", fromTabId: "a" });
    other.postMessage({ kind: "yield", docId: "another", fromTabId: "b" });
    other.postMessage({ kind: "yielded", docId: "doc", fromTabId: "b" });
    other.postMessage(null);
    await vi.advanceTimersByTimeAsync(0);
    expect(a.yields).toEqual([]);
    other.postMessage({ kind: "yield", docId: "doc", fromTabId: "b" });
    await vi.advanceTimersByTimeAsync(0);
    expect(a.yields).toEqual([false]);
    expect(locks.holds(tablesLock("doc"))).toBe(false);
  });

  it("is taken from one that does not answer", async () => {
    const [a, b] = [tab("a", true), tab("b")];
    await a.lock.acquire(false);
    // The clock of signals is not one that can be turned.
    vi.spyOn(AbortSignal, "timeout").mockImplementation((ms) => {
      const controller = new AbortController();
      setTimeout(() => controller.abort(new DOMException("", "TimeoutError")), ms);
      return controller.signal;
    });
    const taken = b.lock.acquire(true);
    await vi.advanceTimersByTimeAsync(2_400);
    expect(b.lock.held).toBe(false);
    await vi.advanceTimersByTimeAsync(200);
    expect(await taken).toBe(true);
    expect(a.yields).toEqual([false, true]);
    expect(a.lock.held).toBe(false);
    expect(b.lock.held).toBe(true);
  });

  it("is not asked for any more once disposed of", async () => {
    const [a, b] = [tab("a"), tab("b")];
    await a.lock.acquire(false);
    a.lock.dispose();
    await vi.advanceTimersByTimeAsync(0);
    expect(await b.lock.acquire(false)).toBe(true);
    a.lock.dispose();
    expect(b.lock.held).toBe(true);
  });
});

describe("deleting the tables of a workbook", () => {
  const stub = (root: FakeDirectory | null) =>
    vi.stubGlobal("navigator", {
      locks,
      storage: root ? { getDirectory: async () => root } : undefined,
    });

  const allTables = async (root: FakeDirectory) =>
    (await root.getDirectoryHandle("reticula-v1", { create: true })).getDirectoryHandle("tables", {
      create: true,
    });

  it("removes them, and is done where there are none", async () => {
    const root = new FakeDirectory();
    stub(root);
    expect(await deleteTables("doc")).toBe(true);
    const tables = await allTables(root);
    await tables.getDirectoryHandle("doc", { create: true });
    await tables.getDirectoryHandle("other", { create: true });
    expect(await deleteTables("doc")).toBe(true);
    expect([...tables.directories.keys()]).toEqual(["other"]);
    expect(await deleteTables("doc")).toBe(true);
    stub(null);
    expect(await deleteTables("doc")).toBe(true);
  });

  it("leaves them while they are open", async () => {
    const root = new FakeDirectory();
    stub(root);
    const tables = await allTables(root);
    await tables.getDirectoryHandle("doc", { create: true });
    const a = tab("a");
    await a.lock.acquire(false);
    expect(await deleteTables("doc")).toBe(false);
    expect([...tables.directories.keys()]).toEqual(["doc"]);
    a.lock.release();
    await vi.advanceTimersByTimeAsync(0);
    expect(await deleteTables("doc")).toBe(true);
  });
});
