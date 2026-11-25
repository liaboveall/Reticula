import type { TableRecord } from "@reticula/protocol/collab-api";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { dequeue, enqueue, type QueuedUpload, queuedUploads } from "./delete.ts";
import { FakeChannel, FakeDirectory, FakeLocks } from "./fake-storage.ts";
import type { TableEngine } from "./host.ts";
import { Uploader, type UploadProgress } from "./uploads.ts";

const PIECE = 8 << 20;

/** A server with one table. */
class Server {
  record: Partial<TableRecord> | null = null;
  /** Bytes that have arrived. */
  arrived = 0;
  /** What was asked, in short. */
  readonly log: string[] = [];
  /** What is answered instead, once each. */
  readonly instead: (Response | "offline" | null)[] = [];
  /** What becomes of completing, once each. */
  readonly completing: number[] = [];

  /** Who is signed in. */
  user: string | null = "user";

  readonly fetch = async (url: string, init: RequestInit): Promise<Response> => {
    if (url === "/api/auth/me") {
      return Response.json(this.user ? { id: this.user } : {}, { status: this.user ? 200 : 401 });
    }
    const path = new URL(url, "http://here/").pathname.replace("/api/workbooks/cloud/tables", "");
    const offset = new Headers(init.headers).get("Upload-Offset");
    this.log.push(`${init.method} ${path || "/"}${offset === null ? "" : ` ${offset}`}`);
    const instead = this.instead.shift();
    if (instead === "offline") {
      throw new TypeError("failed to fetch");
    }
    if (instead) {
      return instead;
    }
    const answer = (body: object, status = 200) => Response.json(body, { status });
    if (init.method === "DELETE") {
      this.record = null;
      return new Response(null, { status: 204 });
    }
    if (path === "") {
      const declared = JSON.parse(String(init.body));
      this.record ??= { ...declared, status: "uploading" };
      const uploadOffset =
        this.record?.status === "uploading" ? { uploadOffset: this.arrived } : {};
      return answer({ ...this.record, ...uploadOffset }, 201);
    }
    if (path.endsWith("/data")) {
      if (Number(offset) !== this.arrived) {
        return answer({ error: "elsewhere", uploadOffset: this.arrived }, 409);
      }
      this.arrived += (init.body as Uint8Array).length;
      return answer({ uploadOffset: this.arrived });
    }
    const bytes = this.record?.bytes ?? 0;
    const status = this.completing.shift() ?? (this.arrived < bytes ? 409 : 200);
    if (status === 422) {
      this.arrived = 0;
      return answer({ error: "not the bytes of the table" }, 422);
    }
    return status === 409
      ? answer({ error: "more", uploadOffset: this.arrived }, 409)
      : answer({ ...this.record, status: "ready" });
  };
}

let root: FakeDirectory;
let server: Server;
let reported: UploadProgress[];
let engine: TableEngine;
let fed: number;

const queue = () => root.at("reticula-v1", "tables-queue");
const files = () => [...queue().files.keys()].sort();

function upload(id: string, change: Partial<QueuedUpload> = {}): QueuedUpload {
  return {
    userId: "user",
    docId: "cloud",
    entry: { id, name: id, rows: 1, cols: 1, bytes: 10 },
    sha256: "",
    queuedAt: 1,
    from: null,
    ...change,
  };
}

async function put(one: QueuedUpload, bytes = 10): Promise<void> {
  const { from, entry } = one;
  const dir = from ? root.at("reticula-v1", "tables", from.docId) : queue();
  dir.putBytes(`${(from ?? entry).id}.rtbl`, new Uint8Array(bytes));
  await enqueue(one);
}

/** Uploads, with time passing as fast as it can. */
async function run(signal = new AbortController().signal, afresh = false): Promise<void> {
  let isOver = false;
  const running = new Uploader(engine, (progress) => reported.push(progress))
    .run("user", afresh, signal)
    .finally(() => {
      isOver = true;
    });
  running.catch(() => {});
  while (!isOver) {
    await vi.advanceTimersByTimeAsync(100);
  }
  await running;
}

const states = (id?: string) =>
  reported.filter((one) => id === undefined || one.id === id).map(({ state }) => state);

beforeEach(() => {
  vi.useFakeTimers();
  root = new FakeDirectory();
  FakeDirectory.room = Number.POSITIVE_INFINITY;
  server = new Server();
  reported = [];
  fed = 0;
  vi.stubGlobal("navigator", { storage: { getDirectory: async () => root } });
  vi.stubGlobal("location", { href: "http://here/app" });
  vi.stubGlobal("fetch", server.fetch);
  engine = {
    TableStep: { Working: 0 },
    TableWork: {
      digest: () => ({
        feed: (bytes: Uint8Array) => {
          fed += bytes.length;
          return true;
        },
        report: () => "5a".repeat(32),
        free: () => {},
      }),
    },
    Table: class {
      fetch() {
        return 0;
      }
      take(_: number, bytes: Uint8Array) {
        this.file?.append(bytes);
        return 0;
      }
      free() {}
      file: { append(bytes: Uint8Array): boolean } | null = null;
    },
  } as unknown as TableEngine;
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("an upload", () => {
  it("is declared, sent in pieces and completed, and is out of the queue then", async () => {
    await put(upload("t"), 2 * PIECE + 5);
    await run();
    expect(server.log).toEqual([
      "POST /",
      "PATCH /t/data 0",
      `PATCH /t/data ${PIECE}`,
      `PATCH /t/data ${2 * PIECE}`,
      "POST /t/complete",
    ]);
    expect(server.record).toMatchObject({
      id: "t",
      name: "t",
      bytes: 2 * PIECE + 5,
      sha256: "5a".repeat(32),
    });
    expect(fed).toBe(2 * PIECE + 5);
    expect(states()).toEqual(["sending", "sending", "sending", "sending", "done"]);
    expect(files()).toEqual([]);
    // Which it need not be had again for.
    expect([...root.at("reticula-v1", "tables-remote", "user").files.keys()]).toEqual([
      "5a".repeat(32),
    ]);
  });

  it("is told of to others once it is done", async () => {
    FakeChannel.all.clear();
    const heard: unknown[] = [];
    new FakeChannel().addEventListener("message", (event) =>
      heard.push((event as MessageEvent).data),
    );
    await put(upload("t"));
    const channel = new FakeChannel() as unknown as BroadcastChannel;
    const uploader = new Uploader(engine, (progress) => reported.push(progress), channel);
    const running = uploader.run("user", false, new AbortController().signal);
    while (!states().includes("done")) {
      await vi.advanceTimersByTimeAsync(100);
    }
    await running;
    expect(heard).toEqual([{ kind: "uploaded", id: "t", sha256: "5a".repeat(32) }]);
  });

  it("of a table of a workbook here leaves that as it is", async () => {
    await put(upload("t", { from: { docId: "local", id: "l" } }));
    await run();
    expect(files()).toEqual([]);
    expect([...root.at("reticula-v1", "tables", "local").files.keys()]).toEqual(["l.rtbl"]);
  });

  it("has a name that is cut between characters", async () => {
    const half = String.fromCharCode(0xd800);
    const chart = String.fromCodePoint(0x1f4c8);
    const name = `${"x".repeat(119)}${chart}y${half}`;
    await put(upload("t", { entry: { ...upload("t").entry, name } }));
    await run();
    expect(server.record?.name).toBe(`${"x".repeat(119)}${chart}`);
    server.record = null;
    server.arrived = 0;
    await put(upload("u", { entry: { ...upload("u").entry, name: `a${half}b` } }));
    await run();
    expect(server.record).toMatchObject({ name: "a b" });
  });

  it("has a name that the server takes", async () => {
    const name = ` a\tb${"x".repeat(200)}`;
    await put(upload("t", { entry: { ...upload("t").entry, name } }));
    await run();
    expect(server.record?.name).toBe(`a b${"x".repeat(117)}`);
  });

  it("goes on where the server says, and not from the start, also in another run", async () => {
    await put(upload("t"), 2 * PIECE);
    const controller = new AbortController();
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      const answer = await server.fetch(url, init);
      if (server.arrived > 0) {
        controller.abort("the tab is closed");
      }
      init.signal?.throwIfAborted();
      return answer;
    });
    expect(await run(controller.signal).catch((error: unknown) => error)).toBe("the tab is closed");
    vi.stubGlobal("fetch", server.fetch);
    expect(files()).toEqual(["t.json", "t.rtbl"]);
    expect(await queuedUploads()).toMatchObject([{ sha256: "5a".repeat(32), isDeclared: true }]);
    // Some more of it arrived than was known here.
    server.arrived = PIECE + 100;
    server.log.length = 0;
    fed = 0;
    await run();
    expect(server.log).toEqual([
      "POST /t/complete",
      `PATCH /t/data ${PIECE + 100}`,
      "POST /t/complete",
    ]);
    expect(fed).toBe(0);
  });

  it("goes on elsewhere when the server has another length", async () => {
    await put(upload("t"), 100);
    server.instead.push(null, Response.json({ uploadOffset: 30 }, { status: 409 }));
    server.arrived = 0;
    const real = server.fetch;
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      const answer = await real(url, init);
      if (answer.status === 409) {
        server.arrived = 30;
      }
      return answer;
    });
    await run();
    expect(server.log).toEqual([
      "POST /",
      "PATCH /t/data 0",
      "PATCH /t/data 30",
      "POST /t/complete",
    ]);
  });

  it("waits as long as the server says, or longer and longer", async () => {
    await put(upload("t"));
    server.instead.push(
      new Response("busy", { status: 429, headers: { "Retry-After": "30" } }),
      "offline",
      new Response("", { status: 503 }),
    );
    const began = Date.now();
    await run();
    expect(states()).toEqual(["waiting", "waiting", "waiting", "sending", "sending", "done"]);
    const waited = Date.now() - began;
    expect(waited).toBeGreaterThanOrEqual(30_000 + 2_000 + 4_000);
  });

  it("starts over once when what arrived is not the table", async () => {
    await put(upload("t"));
    server.completing.push(422);
    await run();
    expect(server.log).toEqual([
      "POST /",
      "PATCH /t/data 0",
      "POST /t/complete",
      "POST /",
      "PATCH /t/data 0",
      "POST /t/complete",
    ]);
    expect(states().at(-1)).toBe("done");

    server.record = null;
    server.arrived = 0;
    server.completing.push(422, 422);
    await put(upload("u"));
    await run();
    expect(reported.at(-1)).toMatchObject({
      id: "u",
      state: "failed",
      failure: { code: "corrupt" },
    });
    expect(files()).toEqual(["u.json", "u.rtbl"]);
  });

  it("that somebody else has deleted is not declared again unless that is asked for", async () => {
    await put(upload("t"));
    server.instead.push(null, new Response("not found", { status: 404 }));
    await run();
    expect(server.log).toEqual(["POST /", "PATCH /t/data 0"]);
    expect(reported.at(-1)).toMatchObject({ state: "failed", failure: { code: "notFound" } });
    expect(files()).toEqual(["t.json", "t.rtbl"]);
    // Nor when the page is loaded again.
    server.log.length = 0;
    server.instead.push(new Response("not found", { status: 404 }));
    await run();
    expect(server.log).toEqual(["POST /t/complete"]);
    expect(reported.at(-1)).toMatchObject({ state: "failed", failure: { code: "notFound" } });
    server.log.length = 0;
    await run(undefined, true);
    expect(server.log[0]).toBe("POST /");
    expect(states().at(-1)).toBe("done");
  });

  it.each([
    [403, "owner table storage quota exceeded", "accountFull"],
    [403, "the workbook has as many tables as it can", "accountFull"],
    [403, "access denied", "denied"],
    [401, "authentication required", "denied"],
    [413, "too large", "fileTooLarge"],
    [507, "server storage capacity reached", "serverFull"],
    [400, "invalid", "refused"],
    [409, '{"error":"another table has this ID"}', "refused"],
  ])("that is refused with %i %s stays in the queue", async (status, error, code) => {
    const body = error.startsWith("{") ? error : JSON.stringify({ error });
    await put(upload("t"));
    await put(upload("u", { queuedAt: 2 }));
    server.instead.push(new Response(body, { status }));
    await run();
    expect(reported[0]).toMatchObject({ id: "t", state: "failed", failure: { code } });
    // And does not keep others from being uploaded.
    expect(states("u").at(-1)).toBe("done");
    expect(files()).toEqual(["t.json", "t.rtbl"]);
  });

  it("ends when it is given up, whatever it waits for", async () => {
    await put(upload("t"), 2 * PIECE);
    server.instead.push(null, "offline");
    const running = run();
    while (!states().includes("waiting")) {
      await vi.advanceTimersByTimeAsync(0);
    }
    await dequeue("t");
    await running;
    expect(server.log).toEqual(["POST /", "PATCH /t/data 0"]);
    expect(files()).toEqual([]);
  });

  it.each([
    ["first", 0],
    ["last", 2],
  ])("ends when it is given up during the %s read of its digest", async (_, at) => {
    await put(upload("t"), 3 << 20);
    const digest = engine.TableWork.digest;
    let reads = 0;
    (engine.TableWork as { digest: unknown }).digest = () => ({
      ...digest(),
      feed: () => {
        if (reads++ === at) {
          void dequeue("t");
        }
        return true;
      },
    });
    await run();
    expect(files()).toEqual([]);
    expect(reported.at(-1)).toMatchObject({ state: "failed", failure: { code: "cancelled" } });
    expect(server.log.filter((line) => !line.startsWith("DELETE"))).toEqual([]);
  });

  it("that the server came to know of while it was given up is deleted there", async () => {
    await put(upload("t"), 100);
    const real = server.fetch;
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      if (init.method === "POST") {
        await dequeue("t");
      }
      return real(url, init);
    });
    await run();
    expect(server.log).toEqual(["POST /", "DELETE /t"]);
    expect(server.record).toBeNull();
    expect(files()).toEqual([]);
  });

  it("that is given up while it is completed leaves no copy", async () => {
    await put(upload("t"), 100);
    const real = server.fetch;
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      if (url.endsWith("/complete")) {
        await dequeue("t");
      }
      return real(url, init);
    });
    await run();
    expect(server.log.at(-1)).toBe("DELETE /t");
    expect(files()).toEqual([]);
    expect(root.at("reticula-v1").directories.has("tables-remote")).toBe(false);
  });

  it.each([
    ["nobody", null],
    ["somebody else", "other"],
  ])("ends when %s is signed in, and goes on when its owner is", async (_, user) => {
    await put(upload("t"), 2 * PIECE);
    await put(upload("u", { queuedAt: 2 }));
    const real = server.fetch;
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      const answer = await real(url, init);
      if (init.method === "PATCH") {
        server.user = user;
      }
      return answer;
    });
    await run();
    expect(server.log).toEqual(["POST /", "PATCH /t/data 0"]);
    expect(reported.at(-1)).toMatchObject({ state: "failed", failure: { code: "cancelled" } });
    expect(files()).toEqual(["t.json", "t.rtbl", "u.json", "u.rtbl"]);
    expect(root.at("reticula-v1").directories.has("tables-remote")).toBe(false);
    server.user = "user";
    server.log.length = 0;
    vi.stubGlobal("fetch", real);
    await run();
    expect(server.log[0]).toBe("POST /t/complete");
  });

  it("leaves no copy when somebody else has signed in meanwhile", async () => {
    await put(upload("t"));
    const real = server.fetch;
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      const answer = await real(url, init);
      if (url.endsWith("/complete")) {
        server.user = "other";
      }
      return answer;
    });
    await run();
    expect(root.at("reticula-v1").directories.has("tables-remote")).toBe(false);
    expect(files()).toEqual(["t.json", "t.rtbl"]);
  });

  it("goes on when an answer breaks off", async () => {
    await put(upload("t"));
    const broken = () =>
      new Response(
        new ReadableStream({ start: (body) => body.error(new TypeError("network error")) }),
      );
    server.instead.push(broken(), null, broken());
    await run();
    expect(states().filter((state) => state === "waiting")).toHaveLength(2);
    expect(states().at(-1)).toBe("done");
  });

  it("of a file that cannot be read is no failure of the server", async () => {
    await put(upload("t", { sha256: "5a" }), 10);
    await put(upload("u", { queuedAt: 2 }));
    const open = queue().getFileHandle.bind(queue());
    vi.spyOn(queue(), "getFileHandle").mockImplementation(async (name, options) => {
      const handle = await open(name, options);
      if (name !== "t.rtbl") {
        return handle;
      }
      const slice = () => ({
        arrayBuffer: () => Promise.reject(new DOMException("gone", "NotReadableError")),
      });
      return { ...handle, getFile: async () => ({ size: 10, slice }) as unknown as File };
    });
    await run();
    expect(states("t").at(-1)).toBe("failed");
    expect(states("u").at(-1)).toBe("done");
  });

  it("that has to wait holds up no other", async () => {
    await put(upload("t"));
    await put(upload("u", { queuedAt: 2 }));
    const real = server.fetch;
    let isBusy = true;
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      if (isBusy && init.method === "POST" && String(init.body).includes('"t"')) {
        return new Response("", { status: 429, headers: { "Retry-After": "30" } });
      }
      const answer = await real(url, init);
      if (url.endsWith("/complete")) {
        isBusy = false;
        server.record = null;
        server.arrived = 0;
      }
      return answer;
    });
    await run();
    const ends = reported.filter(({ state }) => state === "done").map(({ id }) => id);
    expect(ends).toEqual(["u", "t"]);
  });

  it("of something new begins while others wait", async () => {
    await put(upload("t"));
    server.instead.push(new Response("", { status: 429, headers: { "Retry-After": "30" } }));
    const running = run();
    await vi.advanceTimersByTimeAsync(5_000);
    expect(states()).toEqual(["waiting"]);
    await put(upload("u", { queuedAt: 2 }));
    await vi.advanceTimersByTimeAsync(3_000);
    expect(states("u").at(-1)).toBe("done");
    expect(states("t")).toEqual(["waiting"]);
    server.record = null;
    server.arrived = 0;
    await running;
    expect(states("t").at(-1)).toBe("done");
  });

  it("that was refused is tried again when that is asked for, while others wait", async () => {
    await put(upload("t"));
    await put(upload("u", { queuedAt: 2 }));
    server.instead.push(
      new Response("", { status: 507 }),
      new Response("", { status: 429, headers: { "Retry-After": "60" } }),
    );
    const uploader = new Uploader(engine, (progress) => reported.push(progress));
    const { signal } = new AbortController();
    const running = uploader.run("user", false, signal);
    await vi.advanceTimersByTimeAsync(3_000);
    expect(reported.map(({ id, state }) => [id, state])).toEqual([
      ["t", "failed"],
      ["u", "waiting"],
    ]);
    expect(uploader.run("user", true, signal)).toBe(running);
    await vi.advanceTimersByTimeAsync(3_000);
    expect(states("t").at(-1)).toBe("done");
  });

  it("of something that is not there any more fails", async () => {
    await enqueue(upload("t", { from: { docId: "local", id: "gone" } }));
    await run();
    expect(reported).toMatchObject([{ id: "t", state: "failed", failure: { code: "notFound" } }]);
  });
});

describe("the queue", () => {
  it("is worked on in order, for whoever is signed in, with what comes meanwhile", async () => {
    await put(upload("b", { queuedAt: 2 }));
    await put(upload("a", { queuedAt: 1 }));
    await put(upload("other", { queuedAt: 0, userId: "somebody else" }));
    const real = server.fetch;
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      const answer = await real(url, init);
      if (url.endsWith("/complete")) {
        server.record = null;
        server.arrived = 0;
        if (!states("c").length && url.includes("/b/")) {
          await put(upload("c", { queuedAt: 3 }));
        }
      }
      return answer;
    });
    await run();
    const done = reported.filter(({ state }) => state === "done").map(({ id }) => id);
    expect(done).toEqual(["a", "b", "c"]);
    expect(files()).toEqual(["other.json", "other.rtbl"]);
  });

  it("is rid of imports that were cut short, but not of those under way", async () => {
    queue().putBytes("cut.rtbl", new Uint8Array(3));
    queue().putBytes("going.rtbl", new Uint8Array(3));
    await (await queue().getFileHandle("going.rtbl")).createSyncAccessHandle();
    await run();
    expect(files()).toEqual(["going.rtbl"]);
  });

  it("is rid of those whose entry was cut short, and not of one that was just queued", async () => {
    vi.stubGlobal("navigator", {
      storage: { getDirectory: async () => root },
      locks: new FakeLocks(),
    });
    queue().putBytes("cut.rtbl", new Uint8Array(3));
    queue().put("cut.json", "");
    // An import in another tab ends while the queue is looked through.
    queue().putBytes("new.rtbl", new Uint8Array(3));
    const open = await (await queue().getFileHandle("new.rtbl")).createSyncAccessHandle();
    const keys = queue().keys.bind(queue());
    const imported: Promise<void>[] = [];
    vi.spyOn(queue(), "keys").mockImplementation(async function* () {
      yield* keys();
      if (imported.length === 0) {
        imported.push(enqueue(upload("new", { userId: "other" })).then(() => open.close()));
      }
    });
    await run();
    await Promise.all(imported);
    expect(files()).toEqual(["new.json", "new.rtbl"]);
  });

  it("is empty where nothing can be kept", async () => {
    vi.stubGlobal("navigator", {});
    expect(await queuedUploads()).toEqual([]);
  });
});
