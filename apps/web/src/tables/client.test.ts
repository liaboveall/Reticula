import { HEADER_LEN, SHEET_AT } from "@reticula/protocol/viewport";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { RemoteTables } from "../editor/remote-tables.ts";
import { resumeUploads, TableClient, type TableHandlers, type TableWorker } from "./client.ts";
import { dequeue, enqueue } from "./delete.ts";
import { FakeDirectory, FakeLocks } from "./fake-storage.ts";
import type { TableCommand, TableMessage } from "./protocol.ts";
import { closeTables, lastTables, tablesBusy } from "./shell.ts";
import type { UploadProgress } from "./uploads.ts";

class FakeWorker {
  readonly sent: TableCommand[] = [];
  terminated = false;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;
  onmessageerror: (() => void) | null = null;

  postMessage(command: TableCommand): void {
    this.sent.push(command);
  }

  terminate(): void {
    this.terminated = true;
  }

  say(message: TableMessage | ArrayBuffer): void {
    this.onmessage?.({ data: message } as MessageEvent);
  }

  /** Answers what was asked last. */
  answer(answer: unknown): void {
    this.say({ kind: "answer", id: this.lastId, answer });
  }

  get lastId(): number {
    const last = this.sent.at(-1);
    return last && "id" in last ? last.id : -1;
  }
}

function start(handlers: TableHandlers = {}) {
  const worker = new FakeWorker();
  const client = new TableClient(
    { docId: "doc", tabId: "tab" },
    handlers,
    () => worker as unknown as TableWorker,
  );
  return { worker, client };
}

function page(sheet: number): ArrayBuffer {
  const buffer = new ArrayBuffer(HEADER_LEN);
  new DataView(buffer).setUint32(SHEET_AT, sheet, true);
  return buffer;
}

const opened = (sheet: number, view: number) => ({
  sheet,
  view: { id: view, rows: 10, pages: [] },
  info: { rows: 10, columns: [] },
});

const subscription = {
  epoch: 1,
  firstRow: 0,
  rowCount: 64,
  firstCol: 0,
  colCount: 16,
  visible: { firstRow: 0, endRow: 30, firstCol: 0, endCol: 8 },
  resend: true,
};

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("the client of tables", () => {
  it("matches answers and refusals with what was asked", async () => {
    const { worker, client } = start();
    const list = client.acquire();
    const text = client.text(3, 4);
    expect(worker.sent).toEqual([
      { kind: "acquire", id: 1, ask: { takeOver: false } },
      { kind: "text", id: 2, ask: { row: 3, col: 4 } },
    ]);
    worker.say({ kind: "refused", id: 2, failure: { code: "closed" } });
    worker.say({ kind: "answer", id: 1, answer: [] });
    worker.say({ kind: "answer", id: 99, answer: null });
    expect(await list).toEqual([]);
    await expect(text).rejects.toMatchObject({ name: "TableError", code: "closed" });
    client.stop();
  });

  it("hands on the pages of the table that is open, and of no other", async () => {
    const onPage = vi.fn();
    const { worker, client } = start({ onPage });
    worker.say(page(1));
    const open = client.open("t");
    worker.answer(opened(1, 5));
    await open;
    worker.say(page(1));
    worker.say(page(2));
    expect(onPage).toHaveBeenCalledTimes(1);
    expect(client.status()).toMatchObject({ state: "open", pages: 1 });
    client.closeTable().catch(() => {});
    worker.say(page(1));
    expect(onPage).toHaveBeenCalledTimes(1);
    client.stop();
  });

  it("says which view a subscription is to", async () => {
    const { worker, client } = start();
    const open = client.open("t");
    worker.answer(opened(1, 5));
    await open;
    client.subscribe(subscription, 2);
    expect(worker.sent.at(-1)).toEqual({
      kind: "subscribe",
      view: 5,
      frozenCols: 2,
      subscription,
    });
    client.stop();
  });

  it("has the answer to a question that was given up shown if it comes all the same", async () => {
    const onView = vi.fn();
    const { worker, client } = start({ onView });
    const open = client.open("t");
    worker.answer(opened(1, 5));
    await open;
    const asked = { filters: [], order: [{ column: 0, descending: true }] };
    const controller = new AbortController();
    const first = client.query(asked, { signal: controller.signal });
    const firstId = worker.lastId;
    controller.abort();
    const second = client.query({ filters: [], order: [] });
    const secondId = worker.lastId;
    const view = { id: 6, rows: 3, pages: [] };
    // It had been on its way.
    worker.say({ kind: "answer", id: firstId, answer: view });
    worker.say({ kind: "refused", id: secondId, failure: { code: "invalid" } });
    await expect(first).rejects.toMatchObject({ code: "superseded" });
    await expect(second).rejects.toMatchObject({ code: "invalid" });
    expect(onView).toHaveBeenCalledExactlyOnceWith(view, asked);
    client.subscribe(subscription);
    expect(worker.sent.at(-1)).toMatchObject({ kind: "subscribe", view: 6 });
    // Not one whose answer is awaited.
    const third = client.query(asked);
    worker.answer({ id: 7, rows: 3, pages: [] });
    expect(await third).toMatchObject({ id: 7 });
    expect(onView).toHaveBeenCalledTimes(1);
    client.stop();
  });

  it("gives up what is no longer wanted", async () => {
    const { worker, client } = start();
    const controller = new AbortController();
    const onProgress = vi.fn();
    const made = client.sample("s", 10, 1, { signal: controller.signal, onProgress });
    const id = worker.lastId;
    expect(tablesBusy()).toBe(true);
    worker.say({ kind: "progress", id, progress: { share: 0.5 } });
    expect(onProgress).toHaveBeenCalledWith({ share: 0.5 });
    controller.abort();
    expect(worker.sent.at(-1)).toEqual({ kind: "cancel", id });
    worker.say({ kind: "refused", id, failure: { code: "cancelled" } });
    await expect(made).rejects.toMatchObject({ code: "cancelled" });
    expect(tablesBusy()).toBe(false);

    const sent = worker.sent.length;
    await expect(client.sample("s", 10, 1, { signal: controller.signal })).rejects.toMatchObject({
      code: "cancelled",
    });
    expect(worker.sent.length).toBe(sent);
    client.stop();
  });

  it("is told of other tables, of being asked to give them up and of what cannot be read", () => {
    const onCatalog = vi.fn();
    const onYielded = vi.fn();
    const onTableFailed = vi.fn();
    const { worker, client } = start({ onCatalog, onYielded, onTableFailed });
    worker.say({ kind: "catalog", tables: [] });
    worker.say({ kind: "tableFailed", failure: { code: "corrupt" } });
    worker.say({ kind: "yielded", evicted: true });
    expect(onCatalog).toHaveBeenCalledWith([]);
    expect(onTableFailed).toHaveBeenCalledWith({ code: "corrupt" });
    expect(onYielded).toHaveBeenCalledWith(true);
    expect(client.status().state).toBe("yielded");
    expect(worker.terminated).toBe(true);
  });

  it("fails as a whole when the worker does, once", async () => {
    const onFailed = vi.fn();
    const { worker, client } = start({ onFailed });
    const pending = client.text(0, 0);
    worker.say({ kind: "failed", failure: { code: "memory" } });
    await expect(pending).rejects.toMatchObject({ code: "memory" });
    expect(worker.terminated).toBe(true);
    expect(client.stopped).toBe(true);
    expect(client.status()).toMatchObject({ state: "failed", failure: { code: "memory" } });
    worker.onmessageerror?.();
    worker.say(page(0));
    expect(onFailed).toHaveBeenCalledTimes(1);
    await expect(client.text(0, 0)).rejects.toMatchObject({ code: "closed" });
  });

  it("is the one that tests see when it shows a table", async () => {
    const first = start();
    const second = start();
    const open = first.client.open("t");
    first.worker.answer(opened(1, 1));
    await open;
    expect(lastTables()).toBe(first.client);
    second.client.stop();
    first.client.stop();
    expect(lastTables()?.status().state).toBe("closed");
  });

  it("tells a script that fails from one that cannot be loaded", () => {
    class FakeErrorEvent {
      readonly message = "boom";
    }
    vi.stubGlobal("ErrorEvent", FakeErrorEvent);
    const onFailed = vi.fn();
    const { worker } = start({ onFailed });
    worker.onerror?.(new FakeErrorEvent());
    expect(onFailed).toHaveBeenCalledWith({ code: "crashed", detail: "boom" });
  });

  it("lets the worker close its files, but not for ever", async () => {
    vi.useFakeTimers();
    const { worker, client } = start();
    const closing = client.close();
    expect(worker.sent.at(-1)?.kind).toBe("close");
    expect(worker.terminated).toBe(false);
    worker.answer(null);
    await closing;
    expect(worker.terminated).toBe(true);
    await client.close();

    const hung = start();
    const other = closeTables();
    await vi.advanceTimersByTimeAsync(2_499);
    expect(hung.worker.terminated).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    await other;
    expect(hung.worker.terminated).toBe(true);
  });

  it("ends with the page", () => {
    const target = new EventTarget();
    vi.stubGlobal("addEventListener", target.addEventListener.bind(target));
    vi.stubGlobal("removeEventListener", target.removeEventListener.bind(target));
    const onFailed = vi.fn();
    const { worker } = start({ onFailed });
    target.dispatchEvent(new Event("pagehide"));
    target.dispatchEvent(new Event("pagehide"));
    expect(worker.terminated).toBe(true);
    expect(onFailed).toHaveBeenCalledTimes(1);
    expect(onFailed).toHaveBeenCalledWith({ code: "closed" });
  });
});

describe("the client of tables on the server", () => {
  const record = (id: string) => ({
    id,
    name: id,
    rows: 1,
    cols: 1,
    bytes: 10,
    sha256: "5a",
    status: "ready" as const,
    position: 0,
    createdAt: "",
  });

  function startRemote(handlers: TableHandlers = {}) {
    const remote = {
      target: { docId: "doc", userId: "user", canWrite: true },
      tables: [record("a"), record("b")],
      attach: vi.fn(() => () => {}),
      refresh: vi.fn(async () => {}),
      look: vi.fn(async () => {}),
      giveUp: vi.fn(async (_: string) => false),
      change: vi.fn(
        async (
          edit: (tables: readonly ReturnType<typeof record>[]) => readonly { id: string }[],
          ask: () => Promise<unknown>,
        ) => {
          edited.push(edit(remote.tables).map(({ id }) => id));
          await ask();
        },
      ),
    };
    const edited: string[][] = [];
    const worker = new FakeWorker();
    const client = new TableClient(
      { docId: "doc", tabId: "tab", remote: remote as unknown as RemoteTables },
      handlers,
      () => worker as unknown as TableWorker,
    );
    return { worker, client, remote, edited };
  }

  it("says whose they are, and has the list from elsewhere", async () => {
    const { worker, client, remote } = startRemote();
    const list = client.acquire();
    expect(worker.sent.at(-1)).toMatchObject({ ask: { remote: remote.target } });
    worker.answer([]);
    expect(await list).toEqual(remote.tables);
    client.stop();
  });

  it("opens what is listed, as it is listed", async () => {
    const { worker, client } = startRemote();
    await expect(client.open("c")).rejects.toMatchObject({ code: "notFound" });
    expect(worker.sent).toEqual([]);
    client.open("b").catch(() => {});
    expect(worker.sent.at(-1)).toMatchObject({ kind: "open", ask: { record: record("b") } });
    client.stop();
  });

  it("shows changes before the server has made them", async () => {
    const { worker, client, edited } = startRemote();
    const moved = client.move("b", 0);
    expect(edited).toEqual([["b", "a"]]);
    expect(worker.sent.at(-1)).toMatchObject({ kind: "move", ask: { id: "b", position: 0 } });
    worker.say({ kind: "refused", id: worker.lastId, failure: { code: "denied" } });
    await expect(moved).rejects.toMatchObject({ code: "denied" });
    const deleted = client.delete("a");
    expect(edited.at(-1)).toEqual(["b"]);
    await vi.waitFor(() => expect(worker.sent.at(-1)).toMatchObject({ kind: "delete" }));
    worker.answer(null);
    await deleted;
    client.stop();
  });

  it("deletes a table in the queue on the server as well, if that can be had", async () => {
    const { worker, client, remote } = startRemote();
    const refuse = async (code: "offline" | "denied") => {
      await vi.waitFor(() => expect(worker.sent.at(-1)).toMatchObject({ kind: "delete" }));
      worker.say({ kind: "refused", id: worker.lastId, failure: { code } });
    };
    remote.giveUp.mockResolvedValue(true);
    const first = client.delete("a");
    await refuse("offline");
    await first;
    const second = client.delete("b");
    await refuse("denied");
    await expect(second).rejects.toMatchObject({ code: "denied" });
    remote.giveUp.mockResolvedValue(false);
    const third = client.delete("a");
    await refuse("offline");
    await expect(third).rejects.toMatchObject({ code: "offline" });
    client.stop();
  });

  it("has what was imported uploaded, which is listed by then", async () => {
    const { worker, client, remote } = startRemote();
    const imported = client.sample("s", 1, 1);
    worker.answer({ entry: record("c"), report: null });
    await imported;
    expect(remote.look).toHaveBeenCalledOnce();
    client.stop();
  });

  it("closes a table that is lost, and says why", async () => {
    const onTableFailed = vi.fn();
    const { worker, client } = startRemote({ onTableFailed });
    const open = client.open("a");
    worker.answer(opened(1, 1));
    await open;
    await client.lose("b", { code: "notFound" });
    expect(onTableFailed).not.toHaveBeenCalled();
    const lost = client.lose("a", { code: "denied" });
    expect(worker.sent.at(-1)).toMatchObject({ kind: "closeTable" });
    worker.answer(null);
    await lost;
    expect(onTableFailed).toHaveBeenCalledExactlyOnceWith({ code: "denied" });
    client.stop();
  });

  it("looks at the list again when a table fails", () => {
    const { worker, client, remote } = startRemote();
    worker.say({ kind: "tableFailed", failure: { code: "changed" } });
    expect(remote.refresh).toHaveBeenCalledOnce();
    client.stop();
  });
});

describe("uploading what is queued", () => {
  let root: FakeDirectory;
  let locks: FakeLocks;
  let workers: FakeWorker[];

  const put = (id: string, userId = "user") =>
    enqueue({
      userId,
      docId: "cloud",
      entry: { id, name: id, rows: 1, cols: 1, bytes: 10 },
      sha256: "",
      queuedAt: 1,
      from: null,
    });
  const report = (id: string, state: UploadProgress["state"]): UploadProgress => ({
    docId: "cloud",
    id,
    name: id,
    state,
    sent: 0,
    total: 10,
  });
  /** The worker that uploads, once there is one. */
  const started = () =>
    vi.waitFor(() => {
      const worker = workers.at(-1);
      expect(worker?.sent[0]).toMatchObject({ kind: "upload", ask: { userId: "user" } });
      return worker as FakeWorker;
    });

  beforeEach(() => {
    root = new FakeDirectory();
    locks = new FakeLocks();
    workers = [];
    FakeDirectory.room = Number.POSITIVE_INFINITY;
    vi.stubGlobal("navigator", { locks, storage: { getDirectory: async () => root } });
    vi.stubGlobal(
      "Worker",
      class extends FakeWorker {
        constructor() {
          super();
          workers.push(this);
        }
      },
    );
  });

  it("takes a worker for as long as there is something to do", async () => {
    await resumeUploads("user", () => {});
    await put("a", "somebody else");
    await resumeUploads("user", () => {});
    expect(workers).toEqual([]);
    await put("b");
    const progress: string[] = [];
    const done = resumeUploads("user", ({ state }) => progress.push(state));
    expect(resumeUploads("user", () => {})).toBe(done);
    const worker = await started();
    expect(locks.holds("reticula:table-uploads")).toBe(true);
    expect(tablesBusy()).toBe(false);
    worker.say({ kind: "upload", upload: report("b", "sending") });
    await dequeue("b");
    worker.answer(null);
    await done;
    expect(progress).toEqual(["sending"]);
    expect(worker.terminated).toBe(true);
    expect(locks.holds("reticula:table-uploads")).toBe(false);
  });

  it("tries what was refused again when that is asked for, and not before", async () => {
    await put("c");
    const done = resumeUploads("user", () => {});
    const worker = await started();
    worker.say({ kind: "upload", upload: report("c", "failed") });
    worker.answer(null);
    await done;
    await resumeUploads("user", () => {});
    expect(workers).toHaveLength(1);
    const again = resumeUploads("user", () => {}, true);
    await vi.waitFor(() => expect(workers).toHaveLength(2));
    await started();
    workers.at(-1)?.answer(null);
    await again;
  });

  it("has the worker that is at it try again what was refused", async () => {
    await put("f");
    const done = resumeUploads("user", () => {});
    const worker = await started();
    const { lastId } = worker;
    worker.say({ kind: "upload", upload: report("f", "failed") });
    expect(resumeUploads("user", () => {}, true)).toBe(done);
    expect(worker.sent.at(-1)).toMatchObject({ kind: "upload", ask: { afresh: true } });
    worker.say({ kind: "answer", id: lastId, answer: null });
    await done;
    expect(workers).toHaveLength(1);
    // What was given up was not refused.
    const again = resumeUploads("user", () => {});
    await vi.waitFor(() => expect(workers).toHaveLength(2));
    const other = await started();
    const failure = { code: "cancelled" as const };
    other.say({ kind: "upload", upload: { ...report("f", "failed"), failure } });
    other.answer(null);
    await again;
    const third = resumeUploads("user", () => {});
    await vi.waitFor(() => expect(workers).toHaveLength(3));
    await started();
    workers.at(-1)?.answer(null);
    await third;
  });

  it("leaves it to another tab that is at it", async () => {
    await put("d");
    let release = () => {};
    void locks.request(
      "reticula:table-uploads",
      {},
      () =>
        new Promise<void>((resolve) => {
          release = resolve;
        }),
    );
    const done = resumeUploads("user", () => {});
    await new Promise((later) => setTimeout(later, 20));
    expect(workers).toEqual([]);
    await dequeue("d");
    release();
    await done;
    expect(workers).toEqual([]);
  });

  it("ends when the page is put away, and can be taken up again", async () => {
    await put("e");
    const done = resumeUploads("user", () => {});
    const worker = await started();
    worker.say({ kind: "failed", failure: { code: "crashed" } });
    await expect(done).rejects.toMatchObject({ code: "crashed" });
    const again = resumeUploads("user", () => {});
    await vi.waitFor(() => expect(workers).toHaveLength(2));
    await started();
    workers.at(-1)?.answer(null);
    await again;
  });
});
