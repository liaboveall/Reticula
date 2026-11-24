import type { EngineCommand, EngineEvent } from "@reticula/protocol";
import { beforeEach, describe, expect, test, vi } from "vitest";
import type { WorkerOutput } from "./client.ts";
import type { PageSource } from "./page-pump.ts";

const engine = vi.hoisted(() => ({
  commands: [] as EngineCommand[],
  answer: undefined as EngineEvent | undefined,
  freed: false,
}));

vi.mock("@reticula/protocol", () => ({
  Engine: class {
    handle(command: EngineCommand) {
      engine.commands.push(command);
      return engine.answer;
    }
    hasPendingRecalc() {
      return false;
    }
    free() {
      engine.freed = true;
    }
  },
  encodeWalInit: () => new Uint8Array(),
}));

const { DraftPreviewSession } = await import("./draft-preview.ts");

function open() {
  const posted: WorkerOutput[] = [];
  const flushed: PageSource[] = [];
  const session = new DraftPreviewSession({
    post: (message) => posted.push(message),
    postPages: (source) => flushed.push(source),
    setActiveSheet: () => {},
    getActiveSheet: () => 3,
  });
  return { session, posted, flushed };
}

const range = { sheet: 0, firstRow: 0, rowCount: 2, firstCol: 0, colCount: 2 };
const options = {
  query: "x",
  scope: "activeSheet",
  searchIn: "displayValues",
  caseSensitive: false,
  wholeCell: false,
  useRegex: false,
  skipHidden: false,
} as const;

beforeEach(() => {
  engine.commands = [];
  engine.answer = undefined;
  engine.freed = false;
});

describe("a draft preview", () => {
  test("is asked for pages once, and only for what is new", () => {
    const { session, flushed } = open();
    const subscribe = { ...range, kind: "subscribe", epoch: 4, resend: false } as const;
    session.handleCommand(subscribe);
    expect(engine.commands).toEqual([subscribe]);
    expect(flushed).toHaveLength(1);

    session.handleCommand({ ...subscribe, sheet: undefined } as unknown as EngineCommand);
    expect(engine.commands[1]).toEqual({ ...subscribe, sheet: 3 });
  });

  test.each<EngineCommand>([
    { kind: "jumpEdge", sheet: 0, row: 0, col: 0, dir: "down", select: false },
    { kind: "jumpUsedEnd", sheet: 0, select: true },
    { kind: "findAll", sheet: 0, options, maxResults: 10 },
    { kind: "findNext", sheet: 0, fromRow: 0, fromCol: 0, backwards: false, options },
    { kind: "inspectCell", sheet: 0, rowId: "1", colId: "1" },
    { kind: "copyRange", ...range, cut: false },
  ])("answers $kind", (command) => {
    const { session, posted } = open();
    engine.answer = { kind: "searchNavigated", found: undefined };
    session.handleCommand(command);
    expect(engine.commands).toEqual([command]);
    expect(posted).toEqual([engine.answer]);
  });

  test("never cuts", () => {
    const { session } = open();
    session.handleCommand({ kind: "copyRange", ...range, cut: true });
    expect(engine.commands).toEqual([{ kind: "copyRange", ...range, cut: false }]);
  });

  test.each<EngineCommand>([
    { kind: "editCell", sheet: 0, rowId: "1", colId: "1", raw: "1" },
    { kind: "copyFill", ...range, dir: "down" },
    { kind: "undo" },
  ])("rejects $kind", (command) => {
    const { session, posted } = open();
    session.handleCommand(command);
    expect(engine.commands).toEqual([]);
    expect(posted).toEqual([{ kind: "rejected", reason: "Exit AI draft preview before editing." }]);
  });

  test("has no pages left once it is closed", () => {
    const { session, flushed } = open();
    session.close();
    session.flushPages();
    session.handleCommand({ kind: "undo" });
    expect(engine.freed).toBe(true);
    expect(flushed).toEqual([]);
  });
});
