import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
  buildLocalReplayMergeTxns,
  previewLocalReplayMerge,
  type ReplayBundle,
  type ReplayIndex,
} from "./replay-player.ts";

const REPLAY_DIR = resolve(__dirname, "../../public/data/ai-replays");

describe("AI replay bundles & local closure/conflict planner", () => {
  it("loads index.json with all 10 self-contained bundles", () => {
    const index = JSON.parse(
      readFileSync(resolve(REPLAY_DIR, "index.json"), "utf8"),
    ) as ReplayIndex;
    expect(index.items).toHaveLength(10);
    for (const item of index.items) {
      const bundle = JSON.parse(
        readFileSync(resolve(REPLAY_DIR, `${item.id}.json`), "utf8"),
      ) as ReplayBundle;
      expect(bundle.id).toBe(item.id);
      expect(bundle.baseSnapshotB64.length).toBeGreaterThan(20);
      expect(bundle.steps.length).toBe(item.stepCount);
      expect(bundle.diff.hunks.length).toBeGreaterThan(0);
    }
  });

  it("blocks unresolved conflicts and dangling closure references until resolved", () => {
    const bundle = JSON.parse(
      readFileSync(resolve(REPLAY_DIR, "conflict-and-closure-demo.json"), "utf8"),
    ) as ReplayBundle;
    const structHunk = bundle.diff.hunks.find((h) => h.kind === "structural");
    const conflictHunk = bundle.diff.hunks.find((h) => h.hasConflict);
    const formulaHunk = bundle.diff.hunks.find((h) => !h.hasConflict && h.kind === "cell_block");
    expect(structHunk).toBeDefined();
    expect(conflictHunk).toBeDefined();
    expect(formulaHunk).toBeDefined();
    if (!structHunk || !conflictHunk || !formulaHunk) return;

    const allIds = new Set(bundle.diff.hunks.map((h) => h.id));
    const unresolved = previewLocalReplayMerge(bundle, allIds, {}, false);
    expect(unresolved.canMerge).toBe(false);
    expect(unresolved.unresolvedConflicts.length).toBeGreaterThan(0);

    const conflictKey = conflictHunk.cells[0]?.cellKey ?? "";
    // Reject the structural insert while accepting the formula cell (C1 = B2+B3) that depends on it.
    const withoutStruct = new Set([conflictHunk.id, formulaHunk.id]);
    const blocked = previewLocalReplayMerge(
      bundle,
      withoutStruct,
      { [conflictKey]: "take_draft" },
      false,
    );
    expect(blocked.canMerge).toBe(false);
    expect(blocked.closureIssues.length).toBeGreaterThan(0);
    expect(blocked.closureIssues[0]?.targetHunkId).toBe(structHunk.id);

    // Explicitly allowing #REF! unblocks and chooses sanitized txns.
    const allowedRef = previewLocalReplayMerge(
      bundle,
      withoutStruct,
      { [conflictKey]: "take_draft" },
      true,
    );
    expect(allowedRef.canMerge).toBe(true);

    // Accepting the structural insert together unblocks cleanly with zero closure issues.
    const together = previewLocalReplayMerge(
      bundle,
      allIds,
      { [conflictKey]: "take_draft" },
      false,
    );
    expect(together.canMerge).toBe(true);
    expect(together.closureIssues).toHaveLength(0);

    const txns = buildLocalReplayMergeTxns(bundle, allIds, { [conflictKey]: "take_draft" }, false);
    expect(txns.length).toBeGreaterThanOrEqual(2);
  });
});
