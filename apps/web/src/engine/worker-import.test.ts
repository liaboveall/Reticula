import type { EngineCommand } from "@reticula/protocol";
import { afterAll, beforeEach, describe, expect, test, vi } from "vitest";
import type { HostCommand, WorkerEnvelope, WorkerOutput } from "./client.ts";

/** An engine that has one sheet, and imports what it is told to. */
const fakes = vi.hoisted(() => {
  const sheet = (name: string) => ({ sheet: 0, name, walSeq: "0" });
  const info = { kernelVersion: "test" };
  const state = {
    /** What the steps of an import come to, one after the other. */
    steps: [] as (() => unknown)[],
    /** What it takes to begin one, or to read a file. */
    begin: () => {},
    load: () => {},
    /** Changes that are still to be written. */
    frames: [] as Uint8Array[],
    /** What it was told to do. */
    handled: [] as string[],
    /** The workbook that is open in the engine. */
    open: "",
    panicked: false,
  };
  class FakeEngine {
    handle(command: { kind: string }) {
      state.handled.push(command.kind);
      if (command.kind === "openEmpty") {
        state.open = "empty";
        return { kind: "ready", info, sheet: sheet("empty") };
      }
      return undefined;
    }
    startCsvImport(): void {
      state.begin();
    }
    stepCsvImport() {
      return state.steps.shift()?.();
    }
    loadSnapshotAndWal(bytes: Uint8Array) {
      state.load();
      state.open = new TextDecoder().decode(bytes);
      return { info, sheet: sheet(state.open), replayedTxns: 0 };
    }
    encodeSnapshotFull() {
      return new TextEncoder().encode(state.open);
    }
    startSnapshotStream() {
      return 0;
    }
    stepSnapshotStream() {
      return { kind: "finished", totalLen: state.open.length };
    }
    takeSnapshotChunk() {
      return new TextEncoder().encode(state.open);
    }
    abortSnapshotStream(): void {}
    takePage() {}
    takeAuxEvent() {}
    takeCollabEvent() {}
    takePendingWalFrame() {
      return state.frames.shift();
    }
    hasPendingRecalc() {
      return false;
    }
  }
  return {
    state,
    sheet,
    info,
    FakeEngine,
    engines: [] as FakeEngine[],
    /** The logs that are open, by workbook. */
    logs: new Map<string, number>(),
    failWrite: null as string | null,
  };
});

vi.mock("@reticula/protocol/wasm?url", () => ({ default: "" }));
vi.mock("@reticula/protocol", () => ({
  Engine: class extends fakes.FakeEngine {
    constructor() {
      super();
      fakes.engines.push(this);
    }
  },
  HistoryPlayer: class {},
  init: async () => {},
  hasEnginePanicked: () => fakes.state.panicked,
  encodeWalInit: () => Uint8Array.of(0x57),
  inspectWal: () => ({ status: "valid", recordCount: 0, lastSeq: "0", tailTxns: [] }),
}));
vi.mock("./storage.ts", async (importOriginal) => {
  const storage = await importOriginal<typeof import("./storage.ts")>();
  return {
    ...storage,
    openWalHandle: async (docId: string, ...rest: [never, never]) => {
      const wal = await storage.openWalHandle(docId, ...rest);
      const { logs } = fakes;
      logs.set(docId, (logs.get(docId) ?? 0) + 1);
      return {
        readAll: wal.readAll,
        append: wal.append,
        rewrite: wal.rewrite,
        truncate: wal.truncate,
        get byteLength() {
          return wal.byteLength;
        },
        close: () => logs.set(docId, (logs.get(docId) ?? 0) - 1),
      };
    },
    writeDocState: async (docId: string, ...rest: [never, never]) => {
      if (docId === fakes.failWrite) {
        throw Object.assign(new Error("the disk is full"), { name: "QuotaExceededError" });
      }
      return storage.writeDocState(docId, ...rest);
    },
  };
});

/** Locks that nobody else asks for. */
const held = new Set<string>();
const locks = {
  request: async (
    name: string,
    options: LockOptions | (() => unknown),
    run?: (lock: object | null) => unknown,
  ) => {
    if (typeof options === "function") {
      return options();
    }
    if (held.has(name)) {
      if (options.ifAvailable) {
        return run?.(null);
      }
      throw new Error(`${name} is held`);
    }
    held.add(name);
    try {
      return await run?.({});
    } finally {
      held.delete(name);
    }
  },
};

const posted: WorkerOutput[] = [];
const scope = {
  name: "tab",
  onmessage: null as ((event: { data: WorkerEnvelope }) => void) | null,
  postMessage: (message: WorkerOutput) => posted.push(message),
};
vi.stubGlobal("navigator", { locks });
vi.stubGlobal("self", scope);
await import("./worker.ts");
const { listMemoryKeys, readCatalog } = await import("./storage.ts");
await vi.waitFor(() => expect(fakes.engines).toHaveLength(1));

const sendHost = (command: HostCommand) =>
  scope.onmessage?.({ data: { channel: "host", command } });
const send = (command: EngineCommand, opening?: number) =>
  scope.onmessage?.({
    data: { channel: "engine", command, ...(opening === undefined ? {} : { opening }) },
  });
const kinds = () =>
  posted.flatMap((message) => ("kind" in message && message.kind ? [message.kind] : []));
/** Waits until the worker is done with everything it was sent. */
const idle = async () => {
  const before = kinds().filter((kind) => kind === "catalogListed").length;
  sendHost({ kind: "listDocs" });
  await vi.waitFor(() =>
    expect(kinds().filter((kind) => kind === "catalogListed").length).toBeGreaterThan(before),
  );
};
const reasons = () =>
  posted.flatMap((message) =>
    "kind" in message && message.kind === "rejected" ? [message.reason] : [],
  );
const opened = () =>
  posted.flatMap((message) =>
    "kind" in message && message.kind === "docOpened" ? [message.docId] : [],
  );
const lastOpening = () =>
  posted.reduce(
    (last, message) => ("kind" in message && message.kind === "docOpened" ? message.opening : last),
    0,
  );
const openLogs = () => [...fakes.logs].flatMap(([docId, count]) => (count > 0 ? [docId] : []));
const catalog = async () => (await readCatalog("memory")).map(({ id }) => id);
/** Has the engine change its workbook. Returns how much that adds to the log of one that is stored. */
const change = async (docId: string) => {
  const { readMemoryFile } = await import("./storage.ts");
  const size = () => readMemoryFile(`local/${docId}/wal.log`)?.length ?? 0;
  const before = size();
  fakes.state.frames.push(Uint8Array.of(1, 2, 3));
  send({ kind: "editCell", sheet: 0, rowId: "1", colId: "1", raw: "x" });
  await vi.waitFor(() => expect(fakes.state.frames).toEqual([]));
  await idle();
  return size() - before;
};

let made = 0;
const importCsv = (docId: string) =>
  sendHost({
    kind: "importCsv",
    docId,
    title: "CSV",
    sheetName: "Sheet",
    bytes: new ArrayBuffer(8),
  });
const progress = () => ({ kind: "progress", rowsParsed: 65_536, percent: 50 });
const done = (name: string) => () => {
  fakes.state.open = name;
  return { kind: "done", info: fakes.info, sheet: fakes.sheet(name) };
};
const fails = (message: string) => () => {
  throw new Error(message);
};

let before = "";
let wanted = "";
/** Which of the workbooks that were opened is the one before. */
let opening = 0;

beforeEach(async () => {
  fakes.state.steps = [];
  fakes.state.begin = () => {};
  fakes.state.load = () => {};
  fakes.failWrite = null;
  made++;
  before = `before-${made}`;
  wanted = `wanted-${made}`;
  sendHost({ kind: "createDoc", docId: before, title: "Before", template: "empty" });
  await idle();
  opening = lastOpening();
  fakes.state.handled = [];
  posted.length = 0;
});
afterAll(() => {
  vi.unstubAllGlobals();
});

describe("an import of a CSV file", () => {
  test("opens what it has read, in place of what was open", async () => {
    fakes.state.steps = [progress, done("imported")];
    importCsv(wanted);
    await idle();
    expect(kinds().slice(0, 4)).toEqual([
      "csvImportProgress",
      "catalogListed",
      "docOpened",
      "ready",
    ]);
    expect(opened()).toEqual([wanted]);
    expect([...held]).toEqual([`reticula:local:${wanted}`]);
    expect(openLogs()).toEqual([wanted]);
    expect(await catalog()).toEqual(expect.arrayContaining([before, wanted]));
    expect(await change(wanted)).toBe(3);
  });

  test("keeps from it what was meant for the workbook that was open before", async () => {
    fakes.state.steps = [progress, done("imported")];
    importCsv(wanted);
    send({ kind: "editCell", sheet: 0, rowId: "1", colId: "1", raw: "x" }, opening);
    // Nobody misses these, or they are about no workbook.
    send({ kind: "inspectCell", sheet: 0, rowId: "1", colId: "1" }, opening);
    send({ kind: "setEvalEnv", nowSerial: 1, seed: "1" }, opening);
    await idle();
    expect(opened()).toEqual([wanted]);
    expect(fakes.state.handled).toEqual(["setEvalEnv"]);
    expect(reasons()).toEqual(["Nothing was changed: another workbook has been opened meanwhile."]);
    // From then on it can be changed.
    send({ kind: "editCell", sheet: 0, rowId: "1", colId: "1", raw: "x" }, lastOpening());
    await idle();
    expect(fakes.state.handled).toEqual(["setEvalEnv", "editCell"]);
  });

  test("that fails has what was meant for the workbook that is open done", async () => {
    fakes.state.steps = [progress, fails("a quote is never closed")];
    importCsv(wanted);
    send({ kind: "editCell", sheet: 0, rowId: "1", colId: "1", raw: "x" }, opening);
    await idle();
    expect(fakes.state.handled).toEqual(["editCell"]);
  });

  const failures = [
    ["that cannot be begun", () => Object.assign(fakes.state, { begin: fails("not UTF-8") })],
    [
      "that cannot be finished",
      () => Object.assign(fakes.state, { steps: [progress, fails("a quote is never closed")] }),
    ],
  ] as const;
  test.each(failures)("%s leaves what was open as it was", async (_, fail) => {
    fail();
    importCsv(wanted);
    await idle();
    expect(kinds()).not.toContain("failed");
    expect(reasons()).toHaveLength(1);
    expect([...held]).toEqual([`reticula:local:${before}`]);
    expect(openLogs()).toEqual([before]);
    // Nothing is left of it, and the next change is stored.
    expect(await catalog()).not.toContain(wanted);
    expect(listMemoryKeys(`local/${wanted}/`)).toEqual([]);
    expect(await change(before)).toBe(3);
  });

  test("that cannot be stored has what was open before opened again", async () => {
    fakes.state.steps = [done("imported")];
    fakes.failWrite = wanted;
    importCsv(wanted);
    await idle();
    expect(kinds()).not.toContain("failed");
    expect(reasons()).toEqual(["the disk is full"]);
    expect(opened()).toEqual([before]);
    expect(fakes.state.open).toBe("empty");
    expect([...held]).toEqual([`reticula:local:${before}`]);
    expect(openLogs()).toEqual([before]);
    expect(await catalog()).not.toContain(wanted);
    expect(await change(before)).toBe(3);
  });

  test("can follow one that has failed", async () => {
    fakes.state.steps = [fails("a quote is never closed")];
    importCsv(wanted);
    await idle();
    fakes.state.steps = [done("imported")];
    importCsv(wanted);
    await idle();
    expect(opened()).toEqual([wanted]);
    expect(openLogs()).toEqual([wanted]);
  });
});

describe("an import of a workbook's file", () => {
  const importRtcl = () =>
    sendHost({
      kind: "importRtcl",
      docId: wanted,
      title: "Workbook",
      bytes: new TextEncoder().encode("loaded").buffer,
    });

  test("opens what it has read, in place of what was open", async () => {
    importRtcl();
    await idle();
    expect(opened()).toEqual([wanted]);
    expect(fakes.state.open).toBe("loaded");
    expect([...held]).toEqual([`reticula:local:${wanted}`]);
    expect(openLogs()).toEqual([wanted]);
    expect(await change(wanted)).toBe(3);
  });

  test("that cannot be read leaves what was open as it was", async () => {
    fakes.state.load = fails("not a workbook");
    importRtcl();
    await idle();
    expect(reasons()).toEqual(["not a workbook"]);
    expect([...held]).toEqual([`reticula:local:${before}`]);
    expect(openLogs()).toEqual([before]);
    expect(await catalog()).not.toContain(wanted);
    expect(await change(before)).toBe(3);
  });
});

// The last one: the worker stops.
test("an import that breaks the engine stops the worker, and lets go of everything", async () => {
  fakes.state.steps = [
    () => {
      throw new WebAssembly.RuntimeError("unreachable");
    },
  ];
  importCsv(wanted);
  await vi.waitFor(() => expect(kinds()).toContain("failed"));
  expect(reasons()).toEqual([]);
  await vi.waitFor(() => expect([...held]).toEqual([]));
  expect(openLogs()).toEqual([]);
});
