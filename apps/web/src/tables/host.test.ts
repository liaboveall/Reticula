import { HEADER_LEN } from "@reticula/protocol/viewport";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeChannel, FakeDirectory, FakeLocks } from "./fake-storage.ts";
import { type TableEngine, TablesHost } from "./host.ts";
import type { TableCommand, TableMessage, TableRequestKind, TableRequests } from "./protocol.ts";

// The host with everything around it made up: the engine, the storage, the locks.

const TableStep = { Working: 0, Done: 1, NeedInput: 2, Need: 3, Restart: 4, Failed: 5 };
const TableJob = { None: 0, View: 1, Values: 2, Summary: 3 };

/** What the engine is to do, and what it was told. */
interface Script {
  /** Thrown by whatever is called next. */
  trap: Error | null;
  panic: string | undefined;
  /** Why a page cannot be had. */
  unreadable: string | null;
  /** How many steps a question takes. */
  steps: number;
  /** Whether what is asked next comes across something that is damaged, and will come again. */
  damaged: boolean;
  readonly log: string[];
}

function fakeEngine(script: Script): TableEngine {
  const check = () => {
    if (script.trap) {
      throw script.trap;
    }
  };
  class Table {
    #asked: { job: number; left: number; name: string }[] = [];
    #frame = { sheet: 0, epoch: 0 };
    /** Rows on screen. */
    #seen = 0;
    #due: number[] = [];
    readonly #file: object | undefined;
    readonly #len: number;
    /** Bytes that have come from a server. */
    #come: number | null = null;
    #isAsked = false;
    constructor(file: object | undefined, len: number) {
      this.#file = file;
      this.#len = len;
    }
    get #isThere() {
      return this.#file !== undefined || this.#come === this.#len;
    }
    isWaiting() {
      return this.#come !== null && !this.#isThere;
    }
    fetch(_: object | undefined, end: number) {
      this.#come = 0;
      return end;
    }
    have() {
      return this.#file ? this.#len : (this.#come ?? 0);
    }
    plan(requests: number) {
      script.log.push(`plan ${requests}`);
      if (this.#isAsked || this.#isThere) {
        return new Float64Array();
      }
      this.#isAsked = true;
      return Float64Array.of(0, this.#come ?? 0, this.#len);
    }
    take(_: number, bytes: Uint8Array) {
      this.#come = (this.#come ?? 0) + bytes.length;
      return TableStep.Working;
    }
    ended() {
      this.#isAsked = false;
    }
    leaveLog() {
      script.log.push("leave the log");
    }
    open() {
      check();
      // What its directory is in has to be here, which is the first byte.
      return this.#file || this.#come ? TableStep.Done : TableStep.Need;
    }
    answer(job: number) {
      const columns = [{ name: "a", type: { kind: "integer" }, blanks: 0, dictionary: null }];
      return [
        JSON.stringify({ rows: 1_000, columns }),
        "{}",
        '{"values":[],"blanks":0,"complete":true}',
        '{"columns":[]}',
      ][job];
    }
    text() {
      return this.isWaiting() ? undefined : "x";
    }
    #copied = 0;
    beginCopy() {
      this.#copied = 0;
    }
    stepCopy() {
      return ++this.#copied < script.steps ? TableStep.Working : TableStep.Done;
    }
    takeCopy(html: boolean) {
      return html ? "<a>" : "a";
    }
    setMemoryLimit() {}
    subscribe(sheet: number, epoch: number, _: boolean, cells: Uint32Array) {
      const [, first = 0, count = 0, , , seen = 0, end = 0] = cells;
      this.#frame = { sheet, epoch };
      this.#seen = end - seen;
      this.#due = Array.from({ length: Math.ceil(count / 64) }, (__, at) => first / 64 + at);
    }
    toTop() {
      this.#due = Array.from({ length: Math.ceil(this.#seen / 64) }, (_, at) => at);
      this.#seen = 0;
      return 1_000;
    }
    nextPage() {
      check();
      if (this.isWaiting()) {
        return undefined;
      }
      const row = this.#due.shift();
      if (row === undefined) {
        return undefined;
      }
      const { sheet, epoch } = this.#frame;
      script.log.push(`page ${sheet} ${epoch} ${row}`);
      return new Uint8Array(script.unreadable ? 0 : HEADER_LEN);
    }
    #begin(job: number, name: string) {
      script.log.push(`ask ${name}`);
      this.#asked = this.#asked.filter((asked) => asked.job !== job);
      this.#asked.push({ job, left: script.steps, name });
      return TableStep.Working;
    }
    ask(job: number, question: string) {
      const { search = job === TableJob.View ? "view" : "summary" } = JSON.parse(question);
      return this.#begin(job, search);
    }
    cancel(job: number) {
      script.log.push(`cancel ${job}`);
      this.#asked = this.#asked.filter((asked) => asked.job !== job);
    }
    job() {
      return this.#asked[0]?.job ?? TableJob.None;
    }
    share() {
      return 0.5;
    }
    step() {
      check();
      const [asked] = this.#asked;
      if (!asked) {
        return TableStep.Done;
      }
      if (script.damaged) {
        script.damaged = false;
        this.#asked.shift();
        return TableStep.Restart;
      }
      if (--asked.left > 0) {
        return TableStep.Working;
      }
      this.#asked.shift();
      return asked.name === "bad" ? TableStep.Failed : TableStep.Done;
    }
    free() {
      script.log.push("free");
    }
  }
  const work = (step: () => number, rows = () => 0) => ({
    open: () => TableStep.Done,
    step,
    progress: () => [0.5, rows(), 1],
    report: () => "{}",
    free: () => {},
  });
  const TableWork = {
    sample(file: { append(bytes: Uint8Array): boolean }, rows: number) {
      let made = 0;
      return work(
        () => {
          check();
          if (!file.append(Uint8Array.of(1))) {
            script.unreadable = "io";
            return TableStep.Failed;
          }
          return ++made >= rows ? TableStep.Done : TableStep.Working;
        },
        () => made,
      );
    },
    check: () => work(() => TableStep.Done),
    digest: () => ({ feed: () => true, report: () => "5a", free: () => {} }),
  };
  return {
    TableStep,
    TableJob,
    Table,
    TableWork,
    lastTrouble: () => JSON.stringify({ reason: script.unreadable ?? "memory" }),
    panicMessage: () => script.panic,
    crash: () => {
      script.panic = "a crash was asked for";
      throw new WebAssembly.RuntimeError("unreachable");
    },
  } as unknown as TableEngine;
}

const subscription = (epoch: number) => ({
  epoch,
  firstRow: 0,
  rowCount: 128,
  firstCol: 0,
  colCount: 1,
  visible: { firstRow: 0, endRow: 64, firstCol: 0, endCol: 1 },
  resend: true,
});

let root: FakeDirectory;
let locks: FakeLocks;
let script: Script;
let posted: (TableMessage | ArrayBuffer)[];
let host: TablesHost;
let ids: number;

beforeEach(() => {
  root = new FakeDirectory();
  locks = new FakeLocks();
  FakeChannel.all.clear();
  FakeDirectory.room = Number.POSITIVE_INFINITY;
  vi.stubGlobal("navigator", { locks: undefined, storage: { getDirectory: async () => root } });
  script = {
    trap: null,
    panic: undefined,
    unreadable: null,
    steps: 3,
    damaged: false,
    log: [],
  };
  posted = [];
  ids = 0;
  host = new TablesHost({
    docId: "doc",
    tabId: "tab",
    engine: fakeEngine(script),
    memoryBytes: () => 0,
    post: (message) => posted.push(message),
    locks: locks as unknown as LockManager,
    channel: new FakeChannel() as unknown as BroadcastChannel,
    // Node lets messages go before everything else, as long as there are any.
    defer: (run) => setImmediate(run),
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function send<K extends TableRequestKind>(kind: K, ask: TableRequests[K]["ask"]): number {
  const id = ++ids;
  host.handle({ kind, id, ask } as TableCommand);
  return id;
}

const settled = (id: number) =>
  posted.find(
    (message) =>
      !(message instanceof ArrayBuffer) &&
      (message.kind === "answer" || message.kind === "refused") &&
      message.id === id,
  ) as Extract<TableMessage, { kind: "answer" | "refused" }> | undefined;

async function outcome(id: number): Promise<unknown> {
  const message = await vi.waitFor(
    () => {
      const found = settled(id);
      if (!found) {
        throw new Error("not yet");
      }
      return found;
    },
    { interval: 1 },
  );
  return message.kind === "answer" ? message.answer : message.failure.code;
}

const ask = <K extends TableRequestKind>(kind: K, what: TableRequests[K]["ask"]) =>
  outcome(send(kind, what)) as Promise<TableRequests[K]["answer"]>;

const all = <K extends TableMessage["kind"]>(kind: K) =>
  posted.filter(
    (message): message is Extract<TableMessage, { kind: K }> =>
      !(message instanceof ArrayBuffer) && message.kind === kind,
  );

async function openTable(): Promise<TableRequests["open"]["answer"]> {
  await ask("acquire", { takeOver: false });
  const { entry } = await ask("sample", { name: "s", rows: 3, seed: 1 });
  return ask("open", { id: entry.id });
}

async function tableFiles(): Promise<string[]> {
  const top = await root.getDirectoryHandle("reticula-v1");
  const doc = await (await top.getDirectoryHandle("tables")).getDirectoryHandle("doc");
  return [...doc.files.keys()].filter((name) => name.endsWith(".rtbl"));
}

describe("the host of tables", () => {
  it("has pages of what was subscribed to last sent, for the view that is shown", async () => {
    const { sheet, view } = await openTable();
    host.handle({
      kind: "subscribe",
      view: view.id + 1,
      frozenCols: 0,
      subscription: subscription(1),
    });
    host.handle({ kind: "subscribe", view: view.id, frozenCols: 0, subscription: subscription(2) });
    await vi.waitFor(() => expect(posted.filter((m) => m instanceof ArrayBuffer)).toHaveLength(2));
    expect(script.log.filter((line) => line.startsWith("page"))).toEqual([
      `page ${sheet} 2 0`,
      `page ${sheet} 2 1`,
    ]);
  });

  it("gives a new view with the pages to show in place of the old one", async () => {
    const { view } = await openTable();
    host.handle({ kind: "subscribe", view: view.id, frozenCols: 0, subscription: subscription(1) });
    const next = await ask("query", { query: { filters: [], order: [] } });
    expect(next.pages).toHaveLength(1);
    expect(next.id).not.toBe(view.id);
    // What was subscribed to was the view before.
    const before = script.log.length;
    host.handle({ kind: "subscribe", view: view.id, frozenCols: 0, subscription: subscription(2) });
    expect(await ask("text", { row: 0, col: 0 })).toBe("x");
    expect(script.log.slice(before).some((line) => line.startsWith("page"))).toBe(false);
  });

  it("answers the newest question of a kind, and those of other kinds", async () => {
    await openTable();
    const first = send("query", { query: { filters: [], order: [] } });
    const second = send("query", { query: { filters: [], order: [] } });
    const summary = send("summary", { columns: [0] });
    const values = send("values", { column: 0, search: "", limit: 10 });
    expect(await outcome(first)).toBe("superseded");
    expect(await outcome(second)).toMatchObject({ rows: 1_000 });
    expect(await outcome(summary)).toEqual({ columns: [] });
    expect(await outcome(values)).toMatchObject({ complete: true });
  });

  it("gives up a question that is no longer wanted, and goes on with others", async () => {
    await openTable();
    script.steps = Number.POSITIVE_INFINITY;
    const query = send("query", { query: { filters: [], order: [] } });
    const summary = send("summary", { columns: [0] });
    host.handle({ kind: "cancel", id: query });
    expect(await outcome(query)).toBe("cancelled");
    expect(script.log).toContain(`cancel ${TableJob.View}`);
    expect(settled(summary)).toBeUndefined();
    const closing = send("closeTable", {});
    expect(await outcome(closing)).toBeNull();
    expect(await outcome(summary)).toBe("closed");
  });

  it("copies what was asked for last, and not across two views", async () => {
    await openTable();
    const range = { firstRow: 0, rows: 10, firstCol: 0, cols: 1 };
    script.steps = Number.POSITIVE_INFINITY;
    const first = send("copy", { range });
    const second = send("copy", { range });
    expect(await outcome(first)).toBe("superseded");
    script.steps = 3;
    expect(await outcome(second)).toEqual({ tsv: "a", html: "<a>" });
    script.steps = Number.POSITIVE_INFINITY;
    const third = send("copy", { range });
    script.steps = 3;
    const query = send("query", { query: { filters: [], order: [] } });
    script.steps = Number.POSITIVE_INFINITY;
    await outcome(query);
    expect(await outcome(third)).toBe("superseded");
  });

  it("says why a question cannot be answered", async () => {
    await openTable();
    expect(await ask("values", { column: 0, search: "bad", limit: 1 })).toBe("memory");
    expect(await ask("values", { column: 0, search: "good", limit: 1 })).toMatchObject({
      complete: true,
    });
  });

  it("says once that a table cannot be read any further", async () => {
    const { view } = await openTable();
    script.unreadable = "corrupt";
    host.handle({ kind: "subscribe", view: view.id, frozenCols: 0, subscription: subscription(1) });
    await vi.waitFor(() => expect(all("tableFailed")).toHaveLength(1));
    expect(all("tableFailed")[0]?.failure).toEqual({ code: "corrupt" });
    expect(all("failed")).toEqual([]);
  });

  it.each([
    ["a panic", new WebAssembly.RuntimeError("unreachable"), "index out of bounds", "crashed"],
    ["no memory", new WebAssembly.RuntimeError("unreachable executed"), undefined, "memory"],
    [
      "a wild access",
      new WebAssembly.RuntimeError("memory access out of bounds"),
      undefined,
      "crashed",
    ],
    ["a defect of its own", new TypeError("x is not a function"), undefined, "crashed"],
  ])("ends with %s, whatever it was doing", async (_, trap, panic, code) => {
    const { view } = await openTable();
    script.trap = trap;
    script.panic = panic;
    host.handle({ kind: "subscribe", view: view.id, frozenCols: 0, subscription: subscription(1) });
    await vi.waitFor(() => expect(all("failed")).toHaveLength(1));
    expect(all("failed")[0]?.failure.code).toBe(code);
    expect(locks.holds("reticula:tables:doc")).toBe(false);
    // Nothing is run any more, and all files are closed.
    send("crash", {});
    expect(all("failed")).toHaveLength(1);
    const top = await root.getDirectoryHandle("reticula-v1");
    const doc = await (await top.getDirectoryHandle("tables")).getDirectoryHandle("doc");
    expect([...doc.files.values()].some(({ open }) => open)).toBe(false);
  });

  it("ends when a question or an import does", async () => {
    await openTable();
    script.trap = new WebAssembly.RuntimeError("unreachable");
    send("summary", { columns: [0] });
    await vi.waitFor(() => expect(all("failed")[0]?.failure.code).toBe("memory"));
  });

  it("gives up everything for a tab that asks, and leaves nothing half done", async () => {
    await openTable();
    const importing = send("sample", { name: "big", rows: 1_000_000_000, seed: 1 });
    await vi.waitFor(() => expect(all("progress").length).toBeGreaterThan(0));
    expect(await tableFiles()).toHaveLength(2);
    new FakeChannel().postMessage({ kind: "yield", docId: "doc", fromTabId: "other" });
    expect(await outcome(importing)).toBe("yielded");
    await vi.waitFor(() => expect(all("yielded")).toEqual([{ kind: "yielded", evicted: false }]));
    await vi.waitFor(() => expect(locks.holds("reticula:tables:doc")).toBe(false));
    expect(await tableFiles()).toHaveLength(1);
    expect(await ask("sample", { name: "late", rows: 1, seed: 1 })).toBe("yielded");
  });

  it("leaves nothing of an import there is no room for", async () => {
    await openTable();
    FakeDirectory.room = 2;
    const failing = send("sample", { name: "big", rows: 10, seed: 1 });
    // The engine would say that the file takes no more; here the catalogue does.
    expect(await outcome(failing)).toBe("quota");
    expect(await tableFiles()).toHaveLength(1);
  });

  it("closes, with everything that was under way", async () => {
    await openTable();
    script.steps = Number.POSITIVE_INFINITY;
    const query = send("query", { query: { filters: [], order: [] } });
    expect(await ask("close", {})).toBeNull();
    expect(await outcome(query)).toBe("closed");
    expect(locks.holds("reticula:tables:doc")).toBe(false);
    expect(script.log.at(-1)).toBe("free");
  });
});

describe("the host of tables on the server", () => {
  const BYTES = 1_000;
  const record = { bytes: BYTES, sha256: "5a", status: "ready" as const };
  const target = { docId: "doc", userId: "user", canWrite: true };
  let requests: { url: string; init: RequestInit }[];
  /** Answers a request for bytes, or anything else. */
  let answer: (init: RequestInit) => Response | Promise<Response>;

  function bytes(init: RequestInit, body?: ReadableStream): Response {
    const [, from, last] =
      /^bytes=(\d+)-(\d+)$/.exec(new Headers(init.headers).get("Range") ?? "") ?? [];
    return new Response(body ?? new Uint8Array(Number(last) - Number(from) + 1), {
      status: 206,
      headers: { ETag: '"5a"', "Content-Range": `bytes ${from}-${last}/${BYTES}` },
    });
  }

  /** What else of an answer is to come. */
  let rest: ReadableStreamDefaultController<Uint8Array>;

  /** Answers with a little at first, and with the rest when that is sent. */
  function little(init: RequestInit): Response {
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        rest = controller;
        controller.enqueue(new Uint8Array(10));
      },
    });
    return bytes(init, body);
  }

  beforeEach(() => {
    requests = [];
    answer = bytes;
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      requests.push({ url, init });
      return answer(init);
    });
  });

  const openRemote = async (change: Partial<typeof target> = {}) => {
    await ask("acquire", { takeOver: false, remote: { ...target, ...change } });
    return ask("open", { id: "t", record });
  };
  const copies = () => [...root.at("reticula-v1", "tables-remote", "user").files.keys()];

  it("opens one without the lock and without files of its own", async () => {
    expect(await openRemote()).toMatchObject({ info: { rows: 1_000 }, view: { rows: 1_000 } });
    expect(locks.holds("reticula:tables:doc")).toBe(false);
    expect(requests.map(({ url }) => url)).toEqual(["/api/workbooks/doc/tables/t/data"]);
    expect(copies()).toEqual(["5a"]);
    await vi.waitFor(() =>
      expect(all("remote").at(-1)?.remote).toEqual({ have: BYTES, total: BYTES, isStalled: false }),
    );
  });

  it.each([
    ["a viewer", { canWrite: false }],
    ["somebody with a link", { canWrite: false, userId: "" }],
  ])("keeps nothing on the disk for %s", async (_, change) => {
    await openRemote(change);
    expect(root.at("reticula-v1").directories.has("tables-remote")).toBe(false);
  });

  it("leaves the disk when somebody may only look any more", async () => {
    await openRemote();
    await ask("acquire", { takeOver: false, remote: { ...target, canWrite: false } });
    expect(script.log).toContain("leave the log");
    expect(copies()).toEqual([]);
  });

  it.each([
    ["uploading", "uploading"],
    ["missing", "notFound"],
  ] as const)("does not open one that is %s", async (status, code) => {
    await ask("acquire", { takeOver: false, remote: target });
    expect(await ask("open", { id: "t", record: { ...record, status } })).toBe(code);
    expect(requests).toEqual([]);
  });

  it("does not take more into memory than there may be of", async () => {
    await ask("acquire", { takeOver: false, remote: { ...target, canWrite: false } });
    expect(await ask("open", { id: "t", record: { ...record, bytes: 600 << 20 } })).toBe("limit");
  });

  it("says that there is no connection, and opens when there is one", async () => {
    answer = () => Promise.reject(new TypeError("failed to fetch"));
    const ask0 = send("acquire", { takeOver: false, remote: target });
    await outcome(ask0);
    const opening = send("open", { id: "t", record });
    await vi.waitFor(() => expect(settled(opening)).toBeDefined(), { timeout: 4_000 });
    expect(await outcome(opening)).toBe("offline");
    expect(requests).toHaveLength(3);
    answer = bytes;
    expect(await ask("open", { id: "t", record })).toMatchObject({ view: {} });
  });

  it.each([
    [403, "denied"],
    [404, "notFound"],
    [416, "changed"],
  ])("does not open one that the server answers with %i for", async (status, code) => {
    answer = () => new Response("no", { status });
    expect(await openRemote()).toBe(code);
    expect(copies()).toEqual(code === "changed" ? ["5a"] : []);
  });

  it("opens the one that is asked for last", async () => {
    let come = (_: Response) => {};
    answer = () =>
      new Promise((resolve) => {
        come = resolve;
      });
    await ask("acquire", { takeOver: false, remote: target });
    const first = send("open", { id: "t", record });
    await vi.waitFor(() => expect(requests).toHaveLength(1));
    answer = bytes;
    const second = send("open", { id: "u", record: { ...record, sha256: "5a" } });
    expect(await outcome(first)).toBe("superseded");
    expect(await outcome(second)).toMatchObject({ view: {} });
    come(new Response());
  });

  it("has all of it come before something is asked about it, and says how much has", async () => {
    answer = little;
    await openRemote();
    const query = send("query", { query: { filters: [], order: [] } });
    await vi.waitFor(() =>
      expect(all("progress").find(({ id }) => id === query)?.progress).toMatchObject({ have: 10 }),
    );
    expect(script.log).not.toContain("ask view");
    // A newer one takes its place, and another kind waits as well.
    const next = send("query", { query: { filters: [], order: [] } });
    expect(await outcome(query)).toBe("superseded");
    const values = send("values", { column: 0, search: "", limit: 1 });
    host.handle({ kind: "cancel", id: values });
    expect(await outcome(values)).toBe("cancelled");
    expect(script.log).not.toContain("ask ");
    rest.enqueue(new Uint8Array(BYTES - 10));
    expect(await outcome(next)).toMatchObject({ rows: 1_000 });
    expect(script.log.filter((line) => line === "ask view")).toHaveLength(1);
  });

  it("gives up what waits for it when the table is closed", async () => {
    answer = little;
    await openRemote();
    const query = send("query", { query: { filters: [], order: [] } });
    const text = send("text", { row: 0, col: 0 });
    await ask("closeTable", {});
    expect(await outcome(query)).toBe("closed");
    expect(await outcome(text)).toBe("closed");
  });

  it("waits for a text, and for pages, until they are here", async () => {
    answer = little;
    const { view } = await openRemote();
    const text = send("text", { row: 0, col: 0 });
    host.handle({ kind: "subscribe", view: view.id, frozenCols: 0, subscription: subscription(2) });
    const planned = script.log.length;
    await vi.waitFor(() => expect(script.log.length).toBeGreaterThan(planned));
    expect(settled(text)).toBeUndefined();
    expect(posted.some((message) => message instanceof ArrayBuffer)).toBe(false);
    rest.enqueue(new Uint8Array(BYTES - 10));
    expect(await outcome(text)).toBe("x");
    await vi.waitFor(() => expect(posted.filter((m) => m instanceof ArrayBuffer)).toHaveLength(2));
  });

  it("asks again what came across damage", async () => {
    await openRemote();
    await vi.waitFor(() => expect(all("remote").at(-1)?.remote.have).toBe(BYTES));
    script.damaged = true;
    const query = send("query", { query: { filters: [], order: [] } });
    await vi.waitFor(() =>
      expect(script.log.filter((line) => line === "ask view")).toHaveLength(2),
    );
    host.handle({ kind: "subscribe", view: 1, frozenCols: 0, subscription: subscription(1) });
    expect(await outcome(query)).toMatchObject({ rows: 1_000 });
  });

  it("closes a table that may not be had any more", async () => {
    answer = little;
    await openRemote();
    answer = () => new Response("access denied", { status: 403 });
    const query = send("query", { query: { filters: [], order: [] } });
    // The connection ends, and the next request is refused.
    rest.close();
    await vi.waitFor(() => expect(all("tableFailed")).toHaveLength(1), { timeout: 3_000 });
    expect(all("tableFailed")[0]?.failure).toMatchObject({ code: "denied" });
    expect(["denied", "closed"]).toContain(await outcome(query));
    expect(copies()).toEqual([]);
    expect(await ask("text", { row: 0, col: 0 })).toBe("closed");
    expect(all("failed")).toEqual([]);
  });

  it("has the server rename, move, delete and export", async () => {
    await ask("acquire", { takeOver: false, remote: target });
    answer = () => new Response("bytes");
    await ask("rename", { id: "t", name: "n" });
    await ask("move", { id: "t", position: 2 });
    await ask("delete", { id: "t" });
    const file = await ask("export", { id: "t" });
    expect(await file.text()).toBe("bytes");
    expect(
      requests.map(({ url, init }) => [init.method ?? "GET", url.slice(19), init.body ?? null]),
    ).toEqual([
      ["PATCH", "tables/t", '{"name":"n"}'],
      ["PATCH", "tables/t", '{"position":2}'],
      ["DELETE", "tables/t", null],
      ["GET", "tables/t/data", null],
    ]);
    expect(all("catalog")).toEqual([]);
    answer = () => new Response("access denied", { status: 403 });
    expect(await ask("rename", { id: "t", name: "n" })).toBe("denied");
    answer = () => Promise.reject(new TypeError("failed to fetch"));
    expect(await ask("delete", { id: "t" })).toBe("offline");
  });

  it("puts what is imported into the queue, and opens it from there", async () => {
    await ask("acquire", { takeOver: false, remote: target });
    const { entry } = await ask("sample", { name: "s", rows: 3, seed: 1 });
    const queue = root.at("reticula-v1", "tables-queue");
    expect([...queue.files.keys()].sort()).toEqual([`${entry.id}.json`, `${entry.id}.rtbl`]);
    expect(JSON.parse(queue.text(`${entry.id}.json`) ?? "")).toMatchObject({
      userId: "user",
      docId: "doc",
      entry: { name: "s", bytes: 3 },
      from: null,
    });
    vi.stubGlobal(
      "FileReaderSync",
      class {
        readAsArrayBuffer() {
          return new ArrayBuffer(3);
        }
      },
    );
    const uploading = { ...record, bytes: 3, status: "uploading" as const };
    expect(await ask("open", { id: entry.id, record: uploading })).toMatchObject({ view: {} });
    expect(requests).toEqual([]);
    expect(all("remote").at(-1)?.remote).toMatchObject({ have: 3, total: 3 });
  });

  it("imports nothing for a viewer, or where there are no files, and leaves nothing half done", async () => {
    await ask("acquire", { takeOver: false, remote: { ...target, canWrite: false } });
    expect(await ask("sample", { name: "s", rows: 3, seed: 1 })).toBe("denied");
    await ask("acquire", { takeOver: false, remote: target });
    FakeDirectory.room = 2;
    expect(await ask("sample", { name: "s", rows: 3, seed: 1 })).toBe("quota");
    expect([...root.at("reticula-v1", "tables-queue").files.keys()]).toEqual([]);
    vi.stubGlobal("navigator", {});
    expect(await ask("sample", { name: "s", rows: 3, seed: 1 })).toBe("unsupported");
  });

  it("opens a table although the server has no time once", async () => {
    let refusals = 0;
    answer = (...asked) => (refusals++ === 0 ? new Response("", { status: 503 }) : bytes(...asked));
    await ask("acquire", { takeOver: false, remote: target });
    const opening = send("open", { id: "t", record });
    await vi.waitFor(() => expect(settled(opening)).toBeDefined(), { timeout: 2_000 });
    expect(await outcome(opening)).toMatchObject({ view: {} });
  });

  it("does with a table in the queue what it can without the server", async () => {
    await ask("acquire", { takeOver: false, remote: target });
    const { entry } = await ask("sample", { name: "s", rows: 3, seed: 1 });
    const queue = root.at("reticula-v1", "tables-queue");
    const queued = () => JSON.parse(queue.text(`${entry.id}.json`) ?? "");
    answer = () => Promise.reject(new TypeError("failed to fetch"));
    expect((await ask("export", { id: entry.id })).size).toBe(3);
    expect(await ask("rename", { id: entry.id, name: "other" })).toBeNull();
    expect(queued().entry.name).toBe("other");
    expect(requests).toEqual([]);
    // Once the server knows of it, it is told.
    queue.put(`${entry.id}.json`, JSON.stringify({ ...queued(), isDeclared: true }));
    expect(await ask("rename", { id: entry.id, name: "third" })).toBe("offline");
    answer = () => Response.json({});
    expect(await ask("rename", { id: entry.id, name: "fourth" })).toBeNull();
    expect(queued().entry.name).toBe("fourth");
    expect(await ask("move", { id: entry.id, position: 0 })).toBeNull();
  });

  it("has deleted what the server does not have, and says what else is wrong", async () => {
    await ask("acquire", { takeOver: false, remote: target });
    answer = () => new Response("", { status: 404 });
    expect(await ask("delete", { id: "t" })).toBeNull();
    expect(await ask("rename", { id: "t", name: "n" })).toBe("notFound");
    answer = () => new Response("access denied", { status: 403 });
    expect(await ask("delete", { id: "t" })).toBe("denied");
  });

  it("is not broken by an export that breaks off", async () => {
    await ask("acquire", { takeOver: false, remote: target });
    answer = () =>
      new Response(
        new ReadableStream({ start: (body) => body.error(new TypeError("network error")) }),
      );
    expect(await ask("export", { id: "t" })).toBe("offline");
    expect(all("failed")).toEqual([]);
    answer = () => new Response("12345");
    expect((await ask("export", { id: "t" })).size).toBe(5);
  });

  it("makes room for what is imported by deleting copies", async () => {
    const others = root.at("reticula-v1", "tables-remote", "user");
    others.putBytes("77", new Uint8Array(500));
    await openRemote();
    expect(copies().sort()).toEqual(["5a", "77"]);
    FakeDirectory.room = 2;
    expect(await ask("sample", { name: "s", rows: 3, seed: 1 })).toMatchObject({ entry: {} });
    expect(copies()).toEqual([]);
    // What is open goes on out of memory.
    expect(script.log).toContain("leave the log");
    expect(all("tableFailed")).toEqual([]);
  });

  it("keeps no copy of somebody who has signed out in another tab, and uploads nothing", async () => {
    await openRemote();
    expect(copies()).toEqual(["5a"]);
    await ask("sample", { name: "s", rows: 3, seed: 1 });
    let isAsked = false;
    answer = ({ signal }) => {
      isAsked = true;
      return new Promise((_, reject) =>
        signal?.addEventListener("abort", () => reject(signal.reason)),
      );
    };
    const upload = send("upload", { userId: "user", afresh: false });
    await vi.waitFor(() => expect(isAsked).toBe(true));
    new FakeChannel().postMessage({ kind: "signedOut" });
    await vi.waitFor(() => expect(copies()).toEqual([]));
    expect(all("tableFailed")).toEqual([]);
    expect(await outcome(upload)).toBe("closed");
  });

  it("does not delete a table here that is still to be uploaded", async () => {
    await ask("acquire", { takeOver: false });
    const { entry } = await ask("sample", { name: "s", rows: 3, seed: 1 });
    const queue = root.at("reticula-v1", "tables-queue");
    const queued = { entry: { ...entry, id: "cloud" }, from: { docId: "doc", id: entry.id } };
    queue.put("cloud.json", JSON.stringify(queued));
    expect(await ask("delete", { id: entry.id })).toBe("uploading");
    queue.files.clear();
    expect(await ask("delete", { id: entry.id })).toBeNull();
  });
});
