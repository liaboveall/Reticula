import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { TableEntry } from "../catalog.ts";
import type { TableHandlers, TableTask } from "../client.ts";
import type { TableFailure } from "../failure.ts";
import type { ImportedTable } from "../protocol.ts";

const TABLE: TableEntry = { id: "t", name: "orders", rows: 5, cols: 2, bytes: 100 };

/** What stands for a worker and the tab's end of it. */
class FakeClient {
  stopped = false;
  acquired: boolean[] = [];
  refusal: Error | null = null;
  readonly handlers: TableHandlers;

  constructor(_name: unknown, handlers: TableHandlers) {
    this.handlers = handlers;
    clients.push(this);
  }

  async acquire(takeOver: boolean): Promise<readonly TableEntry[]> {
    this.acquired.push(takeOver);
    if (refuse) {
      throw refuse;
    }
    return [TABLE];
  }

  async close(): Promise<void> {
    this.stopped = true;
    await closed;
  }

  crash(failure: TableFailure): void {
    this.stopped = true;
    this.handlers.onFailed?.(failure);
  }
}

let clients: FakeClient[] = [];
let refuse: Error | null = null;
/** When a worker that is closed has let go of everything. */
let closed: Promise<void> = Promise.resolve();

vi.mock("../client.ts", () => ({
  TableClient: class {
    constructor(name: unknown, handlers: TableHandlers) {
      // biome-ignore lint/correctness/noConstructorReturn: a fake in its place
      return new FakeClient(name, handlers);
    }
  },
}));

const { TableError } = await import("../failure.ts");
const { TableSession } = await import("./session.ts");

function open() {
  const onCatalog = vi.fn();
  const session = new TableSession({ docId: "doc", tabId: "tab", onCatalog });
  const user = {
    acceptPage: vi.fn(),
    acceptView: vi.fn(),
    acceptPart: vi.fn(),
    fail: vi.fn(),
  };
  return { session, user, onCatalog };
}

beforeEach(() => {
  vi.useFakeTimers();
  clients = [];
  refuse = null;
  closed = Promise.resolve();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("the worker behind tables", () => {
  test("is started when there is a use for it, once, and lists them", async () => {
    const { session, onCatalog } = open();
    expect(clients).toHaveLength(0);
    const [one, other] = await Promise.all([session.client(), session.client()]);
    expect(one).toBe(other);
    expect(clients).toHaveLength(1);
    expect(clients[0]?.acquired).toEqual([false]);
    expect(onCatalog).toHaveBeenCalledExactlyOnceWith([TABLE]);
  });

  test("is asked again when it was refused, and takes over when it is told to", async () => {
    const { session } = open();
    refuse = new TableError("blocked");
    await expect(session.client()).rejects.toThrow("blocked");
    await expect(session.client()).rejects.toThrow("blocked");
    refuse = null;
    await session.client(true);
    expect(clients).toHaveLength(1);
    expect(clients[0]?.acquired).toEqual([false, false, true]);
  });

  test("is replaced after a crash, by one that takes back what it had", async () => {
    const { session, user } = open();
    session.use(user);
    await session.client();
    clients[0]?.crash({ code: "crashed" });
    expect(user.fail).toHaveBeenCalledExactlyOnceWith({ code: "crashed" });
    await session.client();
    expect(clients).toHaveLength(2);
    expect(clients[1]?.acquired).toEqual([true]);
    // What the one before still has to say is of no interest.
    clients[0]?.handlers.onPage?.(new ArrayBuffer(1));
    clients[0]?.handlers.onFailed?.({ code: "crashed" });
    expect(user.acceptPage).not.toHaveBeenCalled();
    expect(user.fail).toHaveBeenCalledTimes(1);
    // Other tabs may have them by the next time.
    clients[1]?.handlers.onYielded?.(true);
    expect(user.fail).toHaveBeenLastCalledWith({ code: "yielded" });
    await session.client();
    expect(clients[1]?.acquired).toEqual([true, false]);
  });

  test("goes a while after the last table, and comes back", async () => {
    const { session, user } = open();
    session.use(user);
    await session.client();
    vi.advanceTimersByTime(600_000);
    expect(clients[0]?.stopped).toBe(false);
    session.use(null);
    vi.advanceTimersByTime(59_000);
    session.use(user);
    vi.advanceTimersByTime(600_000);
    expect(clients[0]?.stopped).toBe(false);
    session.use(null);
    vi.advanceTimersByTime(60_000);
    expect(clients[0]?.stopped).toBe(true);
    await session.client();
    expect(clients).toHaveLength(2);
    expect(clients[1]?.acquired).toEqual([false]);
  });

  test("tells whoever shows a table of rows that are there after all", async () => {
    const { session, user } = open();
    session.use(user);
    await session.client();
    const view = { id: 2, rows: 3, pages: [] };
    clients[0]?.handlers.onView?.(view, { filters: [], order: [] });
    expect(user.acceptView).toHaveBeenCalledExactlyOnceWith(view, { filters: [], order: [] });
  });

  test("tidies up, unless somebody else has the tables, and then goes at once", async () => {
    const { session } = open();
    session.tidy();
    await vi.advanceTimersByTimeAsync(0);
    expect(clients[0]?.acquired).toEqual([false]);
    expect(clients[0]?.stopped).toBe(false);
    vi.advanceTimersByTime(60_000);
    expect(clients[0]?.stopped).toBe(true);
    refuse = new TableError("blocked");
    session.tidy();
    await vi.advanceTimersByTimeAsync(0);
    expect(clients[1]?.acquired).toEqual([false]);
    expect(clients[1]?.stopped).toBe(true);
  });

  test("is closed with the workbook", async () => {
    const { session } = open();
    await session.client();
    session.close();
    expect(clients[0]?.stopped).toBe(true);
    await expect(session.client()).rejects.toThrow("closed");
    session.reopen();
    await session.client();
    expect(clients).toHaveLength(2);
  });

  test("is waited for that has yet to let go of them", async () => {
    const { session } = open();
    await session.client();
    let letGo = () => {};
    closed = new Promise((resolve) => {
      letGo = resolve;
    });
    session.close();
    const again = open().session;
    const client = again.client();
    await vi.advanceTimersByTimeAsync(1_000);
    expect(clients[1]?.acquired).toEqual([]);
    letGo();
    await client;
    expect(clients[1]?.acquired).toEqual([false]);
  });
});

describe("an import", () => {
  const imported: ImportedTable = { entry: TABLE, report: null };

  test("is shown as it goes on, can be cancelled, and keeps the worker", async () => {
    const { session } = open();
    const seen: unknown[] = [];
    session.subscribe(() => seen.push(session.importing?.progress?.share ?? session.importing));
    let finish = (_: ImportedTable) => {};
    let task: TableTask = {};
    const done = session.import("orders", (_client, given) => {
      task = given;
      return new Promise((resolve) => {
        finish = resolve;
      });
    });
    await vi.advanceTimersByTimeAsync(0);
    task.onProgress?.({ share: 0.5, rows: 10 });
    expect(session.importing).toMatchObject({ name: "orders", progress: { share: 0.5 } });
    await expect(session.import("more", async () => imported)).rejects.toThrow("busy");
    vi.advanceTimersByTime(600_000);
    expect(clients[0]?.stopped).toBe(false);
    session.importing?.cancel();
    expect(task.signal?.aborted).toBe(true);
    finish(imported);
    expect(await done).toBe(imported);
    expect(session.importing).toBeNull();
    expect(seen.at(-1)).toBeNull();
    vi.advanceTimersByTime(60_000);
    expect(clients[0]?.stopped).toBe(true);
  });

  test("that fails leaves room for the next one", async () => {
    const { session } = open();
    refuse = new TableError("unsupported");
    await expect(session.import("orders", async () => imported)).rejects.toThrow("unsupported");
    refuse = null;
    expect(await session.import("orders", async () => imported)).toBe(imported);
  });
});
