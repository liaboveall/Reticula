import type { CollabEvent } from "@reticula/protocol";
import { afterEach, expect, test, vi } from "vitest";
import { startEngine, type WorkerOutput } from "./client.ts";

function fakeWorker() {
  let worker: WorkerStub | undefined;
  class WorkerStub {
    onmessage: ((message: { data: WorkerOutput }) => void) | undefined;
    postMessage = vi.fn();
    terminate = vi.fn();
    constructor() {
      worker = this;
    }
  }
  vi.stubGlobal("Worker", WorkerStub);
  return (event: CollabEvent) => worker?.onmessage?.({ data: { channel: "collab", event } });
}

const initialSync = {
  kind: "open",
  docId: "11111111-1111-1111-1111-111111111111",
  branchId: "00000000-0000-0000-0000-000000000000",
  info: undefined,
  snapshot: new Uint8Array(),
  pendingWal: new Uint8Array(),
} as const;

afterEach(() => vi.unstubAllGlobals());

test("the worker client waits for persistence before dispatching a following send", async () => {
  const receive = fakeWorker();
  const durable = Promise.withResolvers<void>();
  const handled = vi.fn((event: CollabEvent) =>
    event.kind === "appendPending" ? durable.promise : undefined,
  );
  const client = startEngine({
    initialSync,
    onCollab: handled,
    onMessage: vi.fn(),
    onPage: vi.fn(),
  });
  receive({ kind: "appendPending", bytes: Uint8Array.of(1) });
  receive({ kind: "send", bytes: Uint8Array.of(2) });

  await vi.waitFor(() => expect(handled).toHaveBeenCalledTimes(1));
  durable.resolve();
  await vi.waitFor(() =>
    expect(handled.mock.calls.map(([event]) => event.kind)).toEqual(["appendPending", "send"]),
  );
  client.stop();
});

test("a failed WAL write prevents queued sends", async () => {
  const receive = fakeWorker();
  const onMessage = vi.fn();
  const handled = vi.fn(async (event: CollabEvent) => {
    if (event.kind === "appendPending") throw new Error("WAL write failed");
  });
  const client = startEngine({ initialSync, onCollab: handled, onMessage, onPage: vi.fn() });
  receive({ kind: "appendPending", bytes: Uint8Array.of(1) });
  receive({ kind: "send", bytes: Uint8Array.of(2) });

  await vi.waitFor(() =>
    expect(onMessage).toHaveBeenCalledWith({ kind: "failed", message: "WAL write failed" }),
  );
  expect(handled.mock.calls.map(([event]) => event.kind)).toEqual(["appendPending"]);
  client.stop();
});

test("an ordered WAL rewrite can await a fresh checkpoint without blocking its reply", async () => {
  const receive = fakeWorker();
  const entered = Promise.withResolvers<void>();
  const observed: string[] = [];
  const info = {
    epoch: 0,
    replica: 1,
    confirmedSeq: "5",
    confirmedClientSeq: "4",
    nextIdCounter: 3,
  };
  const checkpoint = { info, snapshot: Uint8Array.of(8) };
  const client = startEngine({
    initialSync,
    async onCollab(event) {
      if (event.kind === "replacePending") {
        entered.resolve();
        expect(await client.checkpoint()).toEqual(checkpoint);
      }
      observed.push(event.kind);
    },
    onMessage: vi.fn(),
    onPage: vi.fn(),
  });
  receive({ kind: "replacePending", info, bytes: Uint8Array.of(1) });
  receive({ kind: "send", bytes: Uint8Array.of(2) });
  await entered.promise;
  expect(observed).toEqual([]);
  receive({ kind: "checkpoint", checkpoint });
  await vi.waitFor(() => expect(observed).toEqual(["replacePending", "send"]));
  client.stop();
});
