import type { AgentStepKind, DiffHunkKind } from "@reticula/protocol/collab-api";
import { expect, it, vi } from "vitest";
import {
  closureSource,
  contentText,
  hunkKindText,
  runStatusText,
  runSummaryText,
  stepKindText,
  stepText,
} from "./labels.ts";

/** Marks what goes through the translation of the engine's texts. */
vi.mock("../i18n/engine-text.ts", () => ({ translateEngineText: (text: string) => `«${text}»` }));

// In English they read as the server spells them, which the tests and the
// users know. A server that is newer than the page may have some of its own.

it("shows the states of runs as they are", () => {
  const statuses = ["queued", "running", "waiting_review", "merged", "rejected", "failed"];
  for (const status of [...statuses, "cancelled", "paused_by_server"]) {
    expect(runStatusText(status)).toBe(status);
  }
});

it("shows the kinds of steps as they are", () => {
  const kinds: Readonly<Record<AgentStepKind, true>> = {
    user_prompt: true,
    assistant_thought: true,
    tool_call: true,
    tool_result: true,
    txn_commit: true,
    error: true,
    finish: true,
  };
  for (const kind of [...Object.keys(kinds), "unheard_of"]) {
    expect(stepKindText(kind as AgentStepKind)).toBe(kind);
  }
});

it("shows the kinds of hunks as they are", () => {
  const kinds: Readonly<Record<DiffHunkKind, true>> = {
    structural: true,
    sort: true,
    cell_block: true,
    meta: true,
  };
  for (const kind of [...Object.keys(kinds), "unheard_of"]) {
    expect(hunkKindText(kind as DiffHunkKind)).toBe(kind);
  }
});

it("translates the summary that is the server's, and none of the model's", () => {
  const none = "Completed analysis with no workbook changes.";
  expect(runSummaryText(none)).toBe(`«${none}»`);
  expect(stepText({ summary: none })).toBe(`«${none}»`);
  expect(runSummaryText("Define revenue")).toBe("Define revenue");
});

it("tells what a step is about", () => {
  expect(stepText({ callId: "c1", name: "set_cells", text: "no" })).toBe("set_cells");
  expect(stepText({ name: null, text: "Thinking" })).toBe("Thinking");
  expect(stepText({ prompt: "Sum up" })).toBe("Sum up");
  expect(stepText({ seq: "7" })).toBe('{"seq":"7"}');
  expect(stepText("plain")).toBe("plain");
  expect(stepText(null)).toBe("null");
});

it("tells what a conflicting cell holds, without translating what is in it", () => {
  expect(contentText("=A1", "Sort")).toBe("=A1");
  expect(contentText(null, "Sort")).toBe("Sort");
  expect(contentText(undefined, "")).toBe("(empty)");
  expect(contentText(null, "<deleted row>")).toBe("<deleted row>");
  expect(contentText(null, "<deleted on main>")).toBe("«<deleted on main>»");
});

it("translates where a broken reference is only if that is a label", () => {
  const label = "Sort Sheet1!A1:B9 (9 rows)";
  expect(closureSource({ sourceCell: label, formulaText: label })).toEqual({
    cell: `«${label}»`,
    formula: `«${label}»`,
  });
  expect(closureSource({ sourceCell: "Sheet1!A1", formulaText: "=A2" })).toEqual({
    cell: "Sheet1!A1",
    formula: "=A2",
  });
});
