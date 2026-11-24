import type { CellDiffEntry, DiffHunk } from "@reticula/protocol/collab-api";
import { describe, expect, it } from "vitest";
import { buildHighlights } from "./diff-highlights.ts";

function cell(over: Partial<CellDiffEntry> = {}): CellDiffEntry {
  return {
    address: "B2",
    baseValue: "1",
    cellKey: "0:1:1",
    colPos: 1,
    conflict: false,
    defaultResolution: "take_draft",
    draftValue: "2",
    mainValue: "1",
    rowPos: 1,
    sheetName: "Sheet1",
    ...over,
  };
}

function hunk(over: Partial<DiffHunk> = {}): DiffHunk {
  return {
    cells: [cell()],
    defaultAccepted: true,
    hasConflict: false,
    id: "h1",
    kind: "cell_block",
    label: "Edit Sheet1!B2 (1 cells)",
    sheetId: "0",
    sheetName: "Sheet1",
    ...over,
  };
}

const accepted = new Set(["h1"]);
const kindOf = (changed: DiffHunk, chosen: ReadonlySet<string> = accepted) =>
  buildHighlights([changed], chosen, 0).get("1:1");

describe("buildHighlights", () => {
  it("goes by what becomes of a cell, not by the words of the label", () => {
    const label = "Edit Deleted orders!B2 (1 cells)";
    const cells = [cell({ sheetName: "Deleted orders" })];
    expect(kindOf(hunk({ label, cells, sheetName: "Deleted orders" }))).toBe("added");
  });

  it("marks a cell the draft empties as deleted", () => {
    expect(kindOf(hunk({ cells: [cell({ draftValue: "" })] }))).toBe("deleted");
    expect(kindOf(hunk({ cells: [cell({ draftValue: "", draftFormula: null })] }))).toBe("deleted");
  });

  it("does not take a formula that shows nothing for a deletion", () => {
    expect(kindOf(hunk({ cells: [cell({ draftValue: "", draftFormula: '=""' })] }))).toBe("added");
  });

  it("does not take a format of an empty cell for a deletion", () => {
    const empty = cell({ baseValue: "", mainValue: "", draftValue: "" });
    expect(kindOf(hunk({ kind: "meta", cells: [empty] }))).toBe("added");
  });

  it("marks what is not accepted as deleted, and conflicts before anything else", () => {
    expect(kindOf(hunk(), new Set())).toBe("deleted");
    const conflict = cell({ conflict: true, draftValue: "" });
    expect(kindOf(hunk({ kind: "structural", cells: [conflict] }))).toBe("conflict");
  });

  it("leaves out the other sheets", () => {
    expect(buildHighlights([hunk({ sheetId: "1" })], accepted, 0).size).toBe(0);
  });
});
