import type { TableRecord } from "@reticula/protocol/collab-api";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { QueuedUpload } from "../tables/delete.ts";
import { TableError } from "../tables/failure.ts";
import { FakeDirectory } from "../tables/fake-storage.ts";
import type { RemoteTarget } from "../tables/remote-api.ts";
import { RemoteTables, resumeQueued, type TableReader } from "./remote-tables.ts";

const uploading = vi.fn(async (..._: unknown[]) => {});
const loadUploads = async () => ({ resumeUploads: uploading });

const record = (id: string, change: Partial<TableRecord> = {}): TableRecord => ({
  id,
  name: id,
  rows: 1,
  cols: 1,
  bytes: 10,
  sha256: `sum-${id}`,
  status: "ready",
  position: 0,
  createdAt: "",
  ...change,
});

class FakePage extends EventTarget {
  visibilityState = "visible";
}

let root: FakeDirectory;
let page: FakePage;
let view: EventTarget;
let requests: URL[];
let headers: Headers[];
/** What the server has. */
let listed: { version: string; tables: TableRecord[] };
let role: string;
/** What is answered instead. */
let status: number | "offline" | null;
let opened: RemoteTables[];
let told: string[][];
let roles: (string | null)[];

const copies = () => root.at("reticula-v1", "tables-remote", "user");
const queue = () => root.at("reticula-v1", "tables-queue");

function open(target: Partial<RemoteTarget> = {}): RemoteTables {
  const tables = new RemoteTables(
    { docId: "doc", userId: "user", canWrite: true, ...target },
    {
      loadUploads,
      onTables: (found) => told.push(found.map(({ name }) => name)),
      onRole: (now) => roles.push(now),
    },
  );
  opened.push(tables);
  return tables;
}

/** Has time pass. */
const settle = (ms = 0) => vi.advanceTimersByTimeAsync(ms);

async function start(target: Partial<RemoteTarget> = {}): Promise<RemoteTables> {
  const tables = open(target);
  void tables.start();
  await settle(10);
  return tables;
}

const lists = () => requests.filter(({ pathname }) => pathname.endsWith("/tables"));

function change(tables: TableRecord[]): void {
  listed = { version: `1.${Number(listed.version.slice(2)) + 1}`, tables };
}

beforeEach(() => {
  vi.useFakeTimers();
  root = new FakeDirectory();
  FakeDirectory.room = Number.POSITIVE_INFINITY;
  page = new FakePage();
  view = new EventTarget();
  requests = [];
  headers = [];
  listed = { version: "1.1", tables: [record("a"), record("b")] };
  role = "editor";
  status = null;
  opened = [];
  told = [];
  roles = [];
  uploading.mockClear();
  vi.stubGlobal("navigator", { storage: { getDirectory: async () => root } });
  vi.stubGlobal("document", page);
  vi.stubGlobal("window", view);
  vi.stubGlobal("fetch", async (path: string, init: RequestInit) => {
    const url = new URL(path, "http://here");
    requests.push(url);
    headers.push(new Headers(init.headers));
    if (status === "offline") {
      throw new TypeError("failed to fetch");
    }
    if (status !== null) {
      return Response.json({ error: "no" }, { status });
    }
    if (url.pathname === "/api/auth/me") {
      return Response.json({ id: "user" });
    }
    if (url.pathname.endsWith("/tables")) {
      const { version, tables } = listed;
      const isChanged = url.searchParams.get("since") !== version;
      return Response.json({ version, changed: isChanged, ...(isChanged ? { tables } : {}) });
    }
    return Response.json({ id: "doc", role });
  });
});

afterEach(() => {
  for (const tables of opened) {
    tables.stop();
  }
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("the list of tables on the server", () => {
  it("is had at once, and then every 20 seconds if there is another one", async () => {
    const tables = await start();
    expect(told).toEqual([["a", "b"]]);
    expect(tables.tables).toEqual(listed.tables);
    expect(lists().map(({ search }) => search)).toEqual([""]);
    await settle(20_000);
    expect(lists().map(({ search }) => search)).toEqual(["", "?since=1.1"]);
    expect(told).toHaveLength(1);
    change([record("b"), record("a", { name: "A" }), record("c", { status: "uploading" })]);
    await settle(20_000);
    expect(told.at(-1)).toEqual(["b", "A", "c"]);
    expect(tables.tables[2]?.status).toBe("uploading");
    await settle(20_000);
    expect(told).toHaveLength(2);
    expect(lists().at(-1)?.search).toBe("?since=1.2");
  });

  it("is not asked for while nobody sees it, and at once when somebody does", async () => {
    await start();
    page.visibilityState = "hidden";
    page.dispatchEvent(new Event("visibilitychange"));
    await settle(120_000);
    expect(lists()).toHaveLength(1);
    page.visibilityState = "visible";
    page.dispatchEvent(new Event("visibilitychange"));
    await settle(10);
    expect(lists()).toHaveLength(2);
    await settle(20_000);
    expect(lists()).toHaveLength(3);
    view.dispatchEvent(new Event("online"));
    await settle(10);
    expect(lists()).toHaveLength(4);
  });

  it("is asked for less and less often while the server cannot be had", async () => {
    await start();
    status = "offline";
    const times: number[] = [];
    const began = Date.now();
    for (let tries = 0; tries < 6; tries++) {
      const count = lists().length;
      while (lists().length === count) {
        await settle(1_000);
      }
      times.push(Math.round((Date.now() - began) / 10_000) * 10);
    }
    expect(times).toEqual([20, 60, 140, 300, 600, 900]);
    expect(told).toHaveLength(1);
    status = null;
    await opened[0]?.refresh();
    await settle(20_000);
    expect(lists().length).toBe(9);
  });

  it("is asked for once at a time", async () => {
    const tables = await start();
    void tables.refresh();
    void tables.refresh();
    await settle(10);
    expect(lists()).toHaveLength(2);
  });

  it("is had with the link that was given, and without asking who may do what", async () => {
    await start({ userId: "", shareToken: "token", canWrite: false });
    expect(requests.map(({ pathname }) => pathname)).toEqual(["/api/workbooks/doc/tables"]);
    expect(headers[0]?.get("X-Reticula-Share")).toBe("token");
    expect(root.directories.size).toBe(0);
  });

  it("is not asked for any more once it is stopped", async () => {
    const tables = await start();
    tables.stop();
    page.dispatchEvent(new Event("visibilitychange"));
    view.dispatchEvent(new Event("online"));
    await settle(120_000);
    expect(lists()).toHaveLength(1);
  });

  it("is there without the server, for those who may keep it", async () => {
    await start();
    opened[0]?.stop();
    told = [];
    status = "offline";
    const tables = await start();
    expect(told).toEqual([["a", "b"]]);
    expect(tables.tables).toEqual(listed.tables);
    tables.stop();
    status = null;
    // It need not be had again.
    await start();
    expect(lists().at(-1)?.search).toBe("?since=1.1");
    expect(copies().text("doc.json")).toBe(JSON.stringify(["1.1", listed.tables]));
  });
});

describe("tables that cannot be had any more", () => {
  const reader = () => {
    const lost: string[] = [];
    const retargeted: boolean[] = [];
    const made: TableReader = {
      retarget: async ({ canWrite }) => {
        retargeted.push(canWrite);
      },
      lose: async (id, { code }) => {
        lost.push(`${id} ${code}`);
      },
    };
    return { lost, retargeted, made };
  };

  beforeEach(() => {
    for (const name of ["sum-a", "sum-b", "sum-other"]) {
      copies().put(name, "x");
    }
  });

  const kept = () => [...copies().files.keys()].sort();

  it("are closed, and there is no copy of them any more", async () => {
    const tables = await start();
    const { lost, made } = reader();
    tables.attach(made);
    change([record("b")]);
    await settle(20_000);
    expect(lost).toEqual(["a notFound"]);
    expect(kept()).toEqual(["doc.json", "sum-b", "sum-other"]);
    expect(JSON.parse(copies().text("doc.json") ?? "")).toEqual(["1.2", [record("b")]]);
  });

  it.each([401, 403, 404])("with %i are all closed, and nothing is asked for", async (code) => {
    const tables = await start();
    const { lost, made } = reader();
    const detach = tables.attach(made);
    const gone = reader();
    tables.attach(gone.made)();
    status = code;
    await settle(20_000);
    expect(lost).toEqual(["a denied", "b denied"]);
    expect(gone.lost).toEqual([]);
    expect(kept()).toEqual(["sum-other"]);
    expect(told.at(-1)).toEqual([]);
    expect(roles).toEqual([null]);
    const count = requests.length;
    await settle(600_000);
    expect(requests).toHaveLength(count);
    detach();
  });

  it("are not those of a server that is out of order", async () => {
    await start();
    status = 500;
    await settle(20_000);
    expect(told).toHaveLength(1);
    expect(kept()).toEqual(["doc.json", "sum-a", "sum-b", "sum-other"]);
  });

  it.each([
    ["only be looked at", () => {}],
    ["not be had", () => (status = 403)],
    ["not be had, and are of nobody's workbook", () => (status = 404)],
  ])("are deleted when they may %s since the page was closed", async (_, change) => {
    const before = await start();
    before.stop();
    expect(kept()).toContain("sum-a");
    role = "viewer";
    change();
    await start({ canWrite: false });
    expect(kept()).toEqual(["sum-other"]);
  });

  it("are left on the disk for nobody who may only look", async () => {
    const tables = await start();
    const { retargeted, made } = reader();
    tables.attach(made);
    role = "viewer";
    await settle(20_000);
    expect(roles).toEqual(["viewer"]);
    expect(retargeted).toEqual([false]);
    expect(tables.target.canWrite).toBe(false);
    expect(kept()).toEqual(["sum-other"]);
    // Another role has another list.
    change([record("a")]);
    await settle(20_000);
    expect(kept()).toEqual(["sum-other"]);
    role = "owner";
    await settle(20_000);
    expect(roles).toEqual(["viewer", "owner"]);
    expect(kept()).toEqual(["doc.json", "sum-other"]);
  });

  it("are those of others who were here before", async () => {
    root.at("reticula-v1", "tables-remote", "somebody").put("sum", "x");
    await start();
    expect(root.at("reticula-v1", "tables-remote", "somebody").files.size).toBe(0);
    expect(kept()).toContain("sum-other");
  });
});

describe("changes", () => {
  it("show at once, and are taken back when the server does not make them", async () => {
    const tables = await start();
    let answer = (_: unknown) => {};
    let refuse = (_: unknown) => {};
    const ask = () =>
      new Promise((resolve, reject) => {
        answer = resolve;
        refuse = reject;
      });
    const renamed = (name: string) => (all: readonly TableRecord[]) =>
      all.map((table) => (table.id === "a" ? { ...table, name } : table));
    const first = tables.change(renamed("x"), ask);
    expect(told.at(-1)).toEqual(["x", "b"]);
    change([record("a", { name: "x" }), record("b")]);
    answer(null);
    await settle(10);
    await first;
    expect(told.at(-1)).toEqual(["x", "b"]);
    expect(told).toHaveLength(2);

    const second = tables.change(renamed("y"), ask);
    expect(told.at(-1)).toEqual(["y", "b"]);
    const count = lists().length;
    refuse(new TableError("denied"));
    await expect(second).rejects.toMatchObject({ code: "denied" });
    expect(told.at(-1)).toEqual(["x", "b"]);
    expect(lists()).toHaveLength(count);
  });

  it("that delete a table leave nothing of it here", async () => {
    const tables = await start();
    const lost: string[] = [];
    tables.attach({
      retarget: async () => {},
      lose: async (id) => {
        lost.push(id);
      },
    });
    for (const name of ["sum-a", "sum-b"]) {
      copies().put(name, "x");
    }
    await tables.change(
      (all) => all.filter(({ id }) => id !== "a"),
      async () => {
        expect(told.at(-1)).toEqual(["b"]);
        change([record("b")]);
      },
    );
    expect(lost).toEqual(["a"]);
    expect([...copies().files.keys()].sort()).toEqual(["doc.json", "sum-b"]);
    expect(told.at(-1)).toEqual(["b"]);
  });

  it("are looked for after a list that was asked for before them", async () => {
    const tables = await start();
    void tables.refresh();
    await tables.change(
      (all) => all.filter(({ id }) => id !== "a"),
      async () => change([record("b")]),
    );
    expect(told.at(-1)).toEqual(["b"]);
    expect(lists()).toHaveLength(3);
  });
});

describe("what is to be uploaded", () => {
  const queued = (id: string, change: Partial<QueuedUpload> = {}): string =>
    JSON.stringify({
      userId: "user",
      docId: "doc",
      entry: { id, name: id, rows: 5, cols: 2, bytes: 10 },
      sha256: "",
      queuedAt: 1,
      from: null,
      ...change,
    });

  it("is listed as well, unless the server has it, and uploaded", async () => {
    const tables = await start();
    expect(uploading).not.toHaveBeenCalled();
    queue().put("b.json", queued("b"));
    queue().put("c.json", queued("c"));
    queue().put("d.json", queued("d", { docId: "another" }));
    queue().put("e.json", queued("e", { userId: "somebody else" }));
    await tables.look();
    expect(told.at(-1)).toEqual(["a", "b", "c"]);
    expect(tables.tables[2]).toMatchObject({ id: "c", status: "uploading", rows: 5, bytes: 10 });
    expect(uploading).toHaveBeenLastCalledWith("user", expect.any(Function), false);
    await tables.look(true);
    expect(uploading).toHaveBeenLastCalledWith("user", expect.any(Function), true);
    // What has arrived is looked for.
    const count = lists().length;
    const report = uploading.mock.lastCall?.[1] as (progress: object) => void;
    report({ docId: "doc", id: "c", state: "done", sent: 10 });
    await settle(10);
    expect(lists()).toHaveLength(count + 1);
  });

  it("is listed while the server cannot be had, and is gone at once when given up", async () => {
    status = "offline";
    queue().put("c.json", queued("c"));
    const tables = await start();
    expect(told.at(-1)).toEqual(["c"]);
    expect(uploading).toHaveBeenCalled();
    await tables.change(
      (all) => all.filter(({ id }) => id !== "c"),
      () => tables.giveUp("c"),
    );
    expect(told.at(-1)).toEqual([]);
    expect(tables.tables).toEqual([]);
  });

  it("is given up", async () => {
    const tables = await start();
    queue().put("b.json", queued("b"));
    queue().put("c.json", queued("c"));
    queue().put("c.rtbl", "x");
    await tables.look();
    expect(await tables.giveUp("c")).toBe(true);
    expect(await tables.giveUp("b")).toBe(true);
    expect(await tables.giveUp("a")).toBe(false);
    expect(queue().files.size).toBe(0);
  });

  it("waits while somebody may only look", async () => {
    queue().put("c.json", queued("c"));
    role = "viewer";
    await start({ canWrite: false });
    expect(uploading).not.toHaveBeenCalled();
  });

  it("goes on in any workbook, for whoever is signed in", async () => {
    await resumeQueued(loadUploads, () => {});
    expect(requests).toEqual([]);
    queue().put("c.json", queued("c"));
    const onUpload = () => {};
    await resumeQueued(loadUploads, onUpload);
    expect(uploading).toHaveBeenCalledExactlyOnceWith("user", onUpload, false);
    status = 401;
    await expect(resumeQueued(loadUploads, onUpload)).rejects.toThrow();
    expect(uploading).toHaveBeenCalledOnce();
  });
});
