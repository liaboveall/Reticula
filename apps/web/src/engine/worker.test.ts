import type { EngineCommand, EngineEvent } from "@reticula/protocol";
import { afterAll, beforeEach, describe, expect, test, vi } from "vitest";
import type { WorkerEnvelope, WorkerOutput } from "./client.ts";

/** An engine that is nothing but a queue of pages and a log of commands. */
const fakes = vi.hoisted(() => {
  const clock = { now: 0 };
  class FakeEngine {
    commands: EngineCommand[] = [];
    due: number[] = [];
    handle(command: EngineCommand): EngineEvent | undefined {
      this.commands.push(command);
      if (command.kind === "jumpUsedEnd") {
        return { kind: "edgeJumped", sheet: 0, row: 1, col: 1, select: command.select };
      }
      return undefined;
    }
    handleSync() {
      return { kind: "ready" };
    }
    takePage() {
      const page = this.due.shift();
      if (page === undefined) {
        return undefined;
      }
      clock.now += 1.5;
      return Uint8Array.of(page);
    }
    takeAuxEvent() {}
    takeCollabEvent() {}
    takePendingWalFrame() {}
    hasPendingRecalc() {
      return false;
    }
    free() {}
  }
  class FakePlayer extends FakeEngine {
    hasSnapshot() {
      return false;
    }
    loadEmptyBase() {}
    maxLoadedSeq() {
      return 5n;
    }
    currentSeq() {
      return 5n;
    }
    seekTo() {
      return { sheet: 2 };
    }
  }
  return {
    clock,
    FakeEngine,
    FakePlayer,
    engines: [] as FakeEngine[],
    players: [] as FakePlayer[],
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
  HistoryPlayer: class extends fakes.FakePlayer {
    constructor() {
      super();
      fakes.players.push(this);
    }
  },
  init: async () => {},
  hasEnginePanicked: () => false,
}));

const posted: WorkerOutput[] = [];
const scope = {
  name: "tab",
  onmessage: null as ((event: { data: WorkerEnvelope }) => void) | null,
  postMessage: (message: WorkerOutput) => posted.push(message),
};
vi.stubGlobal("self", scope);
vi.spyOn(performance, "now").mockImplementation(() => fakes.clock.now);
await import("./worker.ts");
await vi.waitFor(() => expect(fakes.engines).toHaveLength(1));

const [engine] = fakes.engines;
if (!engine) {
  throw new Error("the worker has made no engine");
}
const send = (command: EngineCommand) =>
  scope.onmessage?.({ data: { channel: "engine", command } });
const names = () =>
  posted.map((message) =>
    message instanceof ArrayBuffer
      ? new Uint8Array(message)[0]
      : "kind" in message
        ? message.kind
        : message.channel,
  );
const subscribe = {
  kind: "subscribe",
  sheet: 0,
  epoch: 1,
  firstRow: 0,
  rowCount: 64,
  firstCol: 0,
  colCount: 16,
} as const;

beforeEach(() => {
  posted.length = 0;
  engine.commands = [];
});
afterAll(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("the worker", () => {
  test("sends pages in slices that let commands in between", async () => {
    engine.due = [1, 2, 3, 4, 5, 6, 7];
    send(subscribe);
    send({ kind: "jumpUsedEnd", sheet: 0, select: false });
    await vi.waitFor(() => expect(names()).toHaveLength(8));
    // Three pages of 1.5 ms are over the budget of a slice.
    expect(names()).toEqual([1, 2, 3, "edgeJumped", 4, 5, 6, 7]);
  });

  test("asks the history player, and not the live engine, about the past", async () => {
    scope.onmessage?.({
      data: {
        channel: "collab",
        command: {
          kind: "open",
          docId: "",
          branchId: "",
          info: undefined,
          snapshot: new Uint8Array(),
          pendingWal: new Uint8Array(),
        },
      },
    });
    scope.onmessage?.({
      data: {
        channel: "host",
        command: { kind: "historySeek", docId: "d", targetSeq: "5", snapshotSeq: null },
      },
    });
    await vi.waitFor(() => expect(names()).toContain("historyReady"));
    const [player] = fakes.players;
    // What the grid holds is sent again as it was then.
    expect(player?.commands).toEqual([{ ...subscribe, sheet: 2 }]);

    posted.length = 0;
    engine.commands = [];
    if (player) {
      player.due = [9, 10];
    }
    send({ ...subscribe, sheet: 2, epoch: 2, resend: true });
    send({ kind: "jumpUsedEnd", sheet: 2, select: true });
    await vi.waitFor(() => expect(names()).toContain("edgeJumped"));
    expect(names()).toEqual([9, 10, "edgeJumped"]);
    expect(player?.commands.slice(1)).toEqual([
      { ...subscribe, sheet: 2, epoch: 2, resend: true },
      { kind: "jumpUsedEnd", sheet: 2, select: true },
    ]);
    expect(engine.commands).toEqual([]);
  });
});
