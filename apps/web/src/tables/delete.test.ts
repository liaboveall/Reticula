import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  deleteCopies,
  deleteTables,
  dequeue,
  dropCopiesOf,
  enqueue,
  giveUp,
  type QueuedUpload,
  queuedUploads,
  requeue,
} from "./delete.ts";
import { FakeChannel, FakeDirectory, FakeLocks } from "./fake-storage.ts";

let root: FakeDirectory;
let locks: FakeLocks;

const upload = (id: string, from: QueuedUpload["from"], queuedAt = 1): QueuedUpload => ({
  userId: "user",
  docId: "cloud",
  entry: { id, name: id, rows: 1, cols: 1, bytes: 10 },
  sha256: "",
  queuedAt,
  from,
});

beforeEach(() => {
  root = new FakeDirectory();
  locks = new FakeLocks();
  FakeDirectory.room = Number.POSITIVE_INFINITY;
  vi.stubGlobal("navigator", { locks, storage: { getDirectory: async () => root } });
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("what is to be uploaded", () => {
  const queue = () => root.at("reticula-v1", "tables-queue");

  it("is in order, without what cannot be read", async () => {
    expect(await queuedUploads()).toEqual([]);
    await enqueue(upload("b", null, 2));
    await enqueue(upload("a", null, 1));
    queue().put("c.json", "");
    queue().put("d.rtbl", "x");
    expect((await queuedUploads()).map(({ entry }) => entry.id)).toEqual(["a", "b"]);
    await enqueue({ ...upload("a", null, 3), sha256: "5a" });
    expect((await queuedUploads()).map(({ sha256 }) => sha256)).toEqual(["", "5a"]);
    queue().put("a.rtbl", "x");
    await dequeue("a");
    await dequeue("nothing");
    expect([...queue().files.keys()].sort()).toEqual(["b.json", "c.json", "d.rtbl"]);
  });

  it("is changed as long as it is there, and by one at a time", async () => {
    await enqueue(upload("a", null));
    await Promise.all([
      requeue("a", (queued) => ({ ...queued, sha256: "5a" })),
      requeue("a", (queued) => ({ ...queued, isDeclared: true })),
    ]);
    expect(await queuedUploads()).toMatchObject([{ sha256: "5a", isDeclared: true }]);
    await Promise.all([
      requeue("a", (queued) => ({ ...queued, sha256: "77" })),
      dequeue("a"),
      requeue("a", (queued) => ({ ...queued, sha256: "78" })),
    ]);
    expect(await queuedUploads()).toEqual([]);
    expect(queue().files.size).toBe(0);
  });

  it("is given up here and on the server, whatever that says", async () => {
    const asked: string[] = [];
    vi.stubGlobal("fetch", async (url: string, { method }: RequestInit) => {
      asked.push(`${method} ${url}`);
      throw new TypeError("failed to fetch");
    });
    await enqueue(upload("a", null));
    await giveUp("cloud", "a");
    expect(await queuedUploads()).toEqual([]);
    expect(asked).toEqual(["DELETE /api/workbooks/cloud/tables/a"]);
  });

  it("is of no workbook, whatever that is called", async () => {
    await enqueue(upload("a", null));
    for (const name of ["upload-queue", "tables-queue"]) {
      expect(await deleteTables(name)).toBe(true);
    }
    expect(await queuedUploads()).toHaveLength(1);
  });

  it("is not deleted with the workbook it is of", async () => {
    root.at("reticula-v1", "tables", "doc").put("t.rtbl", "x");
    await enqueue(upload("u", { docId: "doc", id: "t" }));
    expect(await deleteTables("doc")).toBe(false);
    expect(await deleteTables("other")).toBe(true);
    await dequeue("u");
    expect(await deleteTables("doc")).toBe(true);
    expect(root.at("reticula-v1", "tables").directories.has("doc")).toBe(false);
  });
});

describe("copies of tables on the server", () => {
  const copies = (user: string) => root.at("reticula-v1", "tables-remote", user);

  beforeEach(() => {
    for (const user of ["a", "b"]) {
      copies(user).put("sum", "x");
      copies(user).put("doc.json", "[]");
    }
  });

  it("are deleted, with the lists of them", async () => {
    await deleteCopies();
    expect(copies("a").files.size + copies("b").files.size).toBe(0);
    // When there are none.
    await deleteCopies();
    vi.stubGlobal("navigator", {});
    await deleteCopies();
  });

  it("are deleted of a workbook that may not be had any more, whoever had it", async () => {
    const list = JSON.stringify(["1.1", [{ sha256: "sum" }, { sha256: "gone" }]]);
    copies("a").put("doc.json", list);
    copies("a").put("other", "x");
    await dropCopiesOf("doc");
    expect([...copies("a").files.keys()]).toEqual(["other"]);
    // The list cannot be read, and says nothing.
    expect([...copies("b").files.keys()]).toEqual(["sum"]);
    vi.stubGlobal("navigator", {});
    await dropCopiesOf("doc");
  });

  it("are deleted a little later when they are in use, and known of until then", async () => {
    vi.useFakeTimers();
    const list = JSON.stringify(["1.1", [{ sha256: "sum" }]]);
    copies("a").put("doc.json", list);
    const open = (isOpen: boolean) => {
      const file = copies("a").files.get("sum");
      if (file) {
        file.open = isOpen;
      }
    };
    open(true);
    // For longer than it is tried: the list stays, for another time.
    const dropped = dropCopiesOf("doc");
    await vi.advanceTimersByTimeAsync(4_000);
    await dropped;
    expect([...copies("a").files.keys()]).toEqual(["sum", "doc.json"]);
    // By a page that closes.
    const again = dropCopiesOf("doc");
    await vi.advanceTimersByTimeAsync(500);
    open(false);
    await vi.advanceTimersByTimeAsync(500);
    await again;
    expect(copies("a").files.size).toBe(0);
  });

  it("are no longer used by any tab once somebody has signed out", async () => {
    FakeChannel.all.clear();
    const heard: unknown[] = [];
    new FakeChannel().addEventListener("message", (event) =>
      heard.push((event as MessageEvent).data),
    );
    vi.stubGlobal("BroadcastChannel", FakeChannel);
    await deleteCopies("a");
    expect(heard).toEqual([]);
    await deleteCopies();
    expect(heard).toEqual([{ kind: "signedOut" }]);
  });

  it("are kept for one", async () => {
    await deleteCopies("a");
    expect([copies("a").files.size, copies("b").files.size]).toEqual([2, 0]);
  });

  it("that are open are waited for, for a while", async () => {
    vi.useFakeTimers();
    const handle = await (await copies("a").getFileHandle("sum")).createSyncAccessHandle();
    const deleted = deleteCopies();
    await vi.advanceTimersByTimeAsync(500);
    expect([...copies("a").files.keys()]).toEqual(["sum"]);
    expect(copies("b").files.size).toBe(0);
    handle.close();
    await vi.advanceTimersByTimeAsync(200);
    await deleted;
    expect(copies("a").files.size).toBe(0);

    copies("a").put("sum", "x");
    await (await copies("a").getFileHandle("sum")).createSyncAccessHandle();
    const given = deleteCopies();
    await vi.advanceTimersByTimeAsync(10_000);
    await given;
    expect(copies("a").files.size).toBe(1);
  });
});
