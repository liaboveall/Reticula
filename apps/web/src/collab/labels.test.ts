import { expect, it } from "vitest";
import { LINK_VIEW_LABEL, personText, shareLinkText, withPersonTexts } from "./labels.ts";

it("calls a share link by its label, or else by what it allows", () => {
  expect(shareLinkText({ label: "Finance", role: "viewer" })).toBe("Finance");
  expect(shareLinkText({ label: "", role: "viewer" })).toBe("Viewer link");
  expect(shareLinkText({ label: "", role: "editor" })).toBe("Editor invite");
  expect(shareLinkText({ label: LINK_VIEW_LABEL, role: "viewer" })).toBe("Link-view access");
});

it("leaves people's names alone", () => {
  for (const name of ["Alice", "Deleted user", "AI Analyst", "Viewer", "Undo: Edit A1"]) {
    expect(personText(name)).toBe(name);
  }
});

it("gives the grid the cursors of others under their shown names, and keeps the rest", () => {
  const presence = {
    replica: 3,
    displayName: "Viewer",
    colorSlot: 1,
    cursorRow: 4,
    cursorCol: 2,
    selection: null,
  };
  const shown = withPersonTexts([presence]);
  expect(shown).toEqual([presence]);
  expect(shown[0]).not.toBe(presence);
});
