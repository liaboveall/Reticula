import type { Frame, Theme } from "@reticula/grid/internals";
import { describe, expect, it } from "vitest";
import { buildCase } from "./cases.ts";
import { mutated, withoutText } from "./mutations.ts";
import { CASE_NAMES, CASES, SCALES } from "./names.ts";

// Building a frame reads nothing of a theme but hands it on.
const THEME = { background: "#fff", line: "#eee", selectionFill: "#def" } as Theme;

describe("the cases", () => {
  it.each(CASE_NAMES)("%s shows cells at every scale", (name) => {
    for (const scale of SCALES) {
      const { frame, pages } = buildCase(name, THEME, scale);
      const { visible, scroll, size } = frame;
      expect(visible.endRow).toBeGreaterThan(visible.firstRow);
      expect(visible.endCol).toBeGreaterThan(visible.firstCol);
      for (const position of [scroll.top, scroll.left, size.width, size.height]) {
        expect(position * scale).toBeCloseTo(Math.round(position * scale), 6);
      }
      for (const page of pages) {
        expect(frame.cache.pageOf(page.firstRow, page.firstCol)).toBe(page);
      }
    }
  });

  it("are named so that their images can be files", () => {
    for (const name of CASE_NAMES) {
      expect(name).toMatch(/^[a-z0-9-]+$/);
    }
  });

  it("have a golden image in both themes and at every scale", () => {
    const infos = Object.values(CASES);
    expect(new Set(infos.map((info) => info.dark)).size).toBe(2);
    expect(new Set<number>(infos.map((info) => info.scale))).toEqual(new Set(SCALES));
  });

  it("draw every overlay", () => {
    const overlays: (keyof Frame)[] = [
      "selection",
      "selectionRange",
      "selectionRanges",
      "formulaReferences",
      "remotePresences",
      "diffHighlights",
      "fillPreview",
      "clipboardRange",
      "reorderGuide",
      "filterState",
      "filterRowMap",
      "activeSpillRange",
    ];
    const frames = CASE_NAMES.map((name) => buildCase(name, THEME, 1).frame);
    for (const overlay of overlays) {
      expect(
        frames.some((frame) => frame[overlay]),
        overlay,
      ).toBe(true);
    }
  });

  it("send text that reaches into a page with it", () => {
    const { pages } = buildCase("overflow-across-pages", THEME, 1);
    expect(pages.filter((page) => page.overflows.length > 0).map((page) => page.firstCol)).toEqual([
      16,
    ]);
  });
});

describe("the mutations", () => {
  it("leave out one cell's text, and nothing else", () => {
    const built = buildCase("plain-text", THEME, 1);
    const changed = mutated(built, "text-run");
    let different = 0;
    for (const page of built.pages) {
      const other = changed.cache.pageOf(page.firstRow, page.firstCol);
      different += page.texts.filter((text, index) => text !== other?.texts[index]).length;
    }
    expect(different).toBe(1);
    expect(changed.styles).toEqual(built.frame.styles);
  });

  it.each([
    ["cell-background", "styles", "fillColor"],
    ["border", "borders", "borderTop"],
  ] as const)("leave out one %s", (mutation, name, property) => {
    const built = buildCase(name, THEME, 1);
    const changed = mutated(built, mutation);
    const page = built.pages[0];
    const other = changed.cache.pageOf(0, 0);
    const different = [...(page?.styleIds ?? [])].flatMap((id, index) => {
      const before = built.frame.styles?.[id]?.[property];
      const after = changed.styles?.[other?.styleIds[index] ?? 0]?.[property];
      return before === after ? [] : [after];
    });
    expect(different).toEqual([property === "fillColor" ? undefined : "none"]);
  });

  it("take all text away for finding its ink", () => {
    const frame = withoutText(buildCase("overflow-across-pages", THEME, 1));
    const page = frame.cache.pageOf(0, 16);
    expect(page?.texts.join("")).toBe("");
    expect(page?.overflows).toEqual([]);
  });
});
