import { FLAG_NUMBER } from "@reticula/protocol/viewport";
import { describe, expect, test } from "vitest";
import { FilterRowMap } from "../filter.ts";
import { type SheetLayout, SparseAxis, viewMetrics } from "../layout.ts";
import { ZOOM_STEPS } from "../zoom.ts";
import { packColor } from "./color.ts";
import {
  axes,
  cacheOf,
  colors,
  frameOf,
  METRICS,
  measure,
  PLAIN,
  rectsOf,
  sceneOf,
  theme,
} from "./fixtures.ts";
import type { DiffHighlightMap, Frame } from "./frame.ts";
import { FIXED } from "./look.ts";
import { type Scene, SceneBuilder } from "./scene.ts";

const packed = (css: string) => colors.parse(css);
const sprites = (scene: Scene) => scene.steps.filter((step) => step.kind === "sprite");
const rectsIn = (scene: Scene, color: number) =>
  rectsOf(scene).filter((rect) => rect.color === color);

const labels = (scene: Scene) => {
  const step = scene.steps.find((candidate) => candidate.kind === "labels");
  if (step?.kind !== "labels") {
    throw new Error("no labels");
  }
  const { runs } = step;
  return runs.texts.map((text, run) => ({
    text,
    x: runs.x[run] ?? 0,
    color: runs.color[run] ?? 0,
    clip: runs.clipped[run] === 1 ? [...runs.clip.subarray(run * 4, run * 4 + 4)] : null,
  }));
};

const FROZEN = { layout: axes([], [], 1, 1), scroll: { top: 10, left: 5 } };

describe("a collaborator", () => {
  const ada = { replica: 7, displayName: "Ada", colorSlot: 3 };

  test("has a selection across the panes and one name plate at the cursor in view", () => {
    const scene = sceneOf(
      frameOf({
        ...FROZEN,
        remotePresences: [
          {
            ...ada,
            cursorRow: 0,
            cursorCol: 0,
            selection: { firstRow: 0, endRow: 3, firstCol: 0, endCol: 3 },
          },
        ],
      }),
    );
    expect(rectsIn(scene, packed(theme.refFills[3] ?? ""))).toHaveLength(4);
    const plates = sprites(scene).filter((step) => step.spec.kind === "plate");
    expect(plates).toHaveLength(1);
    const width = Math.ceil(measure.width("600 11px sans-serif", "Ada")) + 12;
    expect(plates[0]).toMatchObject({
      x: 46,
      y: 31,
      spec: {
        width,
        height: 18,
        text: "Ada",
        font: "600 11px sans-serif",
        fill: packed(theme.background),
        stroke: packed(theme.refStrokes[3] ?? ""),
      },
    });
  });

  test("keeps a selection in view when the cursor is out of it", () => {
    const scene = sceneOf(
      frameOf({
        ...FROZEN,
        remotePresences: [
          {
            ...ada,
            cursorRow: 30,
            cursorCol: 30,
            selection: { firstRow: 1, endRow: 31, firstCol: 1, endCol: 31 },
          },
        ],
      }),
    );
    expect(rectsIn(scene, packed(theme.refFills[3] ?? ""))).toHaveLength(1);
    expect(sprites(scene).map((step) => step.spec.kind)).toEqual(["corner"]);
  });

  test("that is the analyst has dashes and a plate of its own", () => {
    const cursor = { cursorRow: 2, cursorCol: 1 };
    const solid = sceneOf(frameOf({ remotePresences: [{ ...ada, ...cursor }] }));
    const dashed = sceneOf(frameOf({ remotePresences: [{ ...ada, ...cursor, colorSlot: 0x83 }] }));
    expect(rectsIn(solid, packed(theme.refStrokes[3] ?? ""))).toHaveLength(4);
    expect(rectsIn(dashed, FIXED.ai).length).toBeGreaterThan(10);
    expect(sprites(dashed).find((step) => step.spec.kind === "plate")?.spec).toMatchObject({
      text: "AI",
      fill: FIXED.ai,
    });
  });
});

describe("a filtered sheet", () => {
  const frame = frameOf({
    ...FROZEN,
    filterState: {
      headerRow: 0,
      lastRow: 10,
      firstCol: 0,
      lastCol: 2,
      activeCols: [1],
      hiddenRowSpans: [[1, 3]],
    },
    filterRowMap: new FilterRowMap(20, [[1, 3]]),
  });

  test("has the numbers of the rows that are left, in the filter's colour", () => {
    const rows = labels(sceneOf(frame));
    const texts = rows.map((label) => label.text);
    expect(texts).toEqual(expect.arrayContaining(["1", "5", "6"]));
    for (const hidden of ["2", "3", "4"]) {
      expect(texts).not.toContain(hidden);
    }
    const colorOf = (text: string) => rows.find((label) => label.text === text)?.color;
    expect(colorOf("5")).toBe(packed(theme.filterHeaderText));
    expect(colorOf("A")).toBe(packed(theme.headerText));
  });

  test("has a line where rows are hidden", () => {
    const unfrozen = { ...frame, layout: axes(), scroll: { top: 0, left: 0 } };
    const lines = rectsIn(sceneOf(unfrozen), packed(theme.filterHeaderText)).filter(
      (rect) => rect.left === 0 && rect.height === 2,
    );
    // Over the edge between the header row and the first row that is left.
    expect(lines).toContainEqual(expect.objectContaining({ top: 49, width: METRICS.headerWidth }));
  });

  test("has a button in each header cell, with a funnel where a filter is in force", () => {
    const scene = sceneOf(frame);
    const signs = sprites(scene).filter((step) => step.spec.kind !== "corner");
    expect(signs.map((step) => [step.spec.kind, step.spec.color])).toEqual([
      ["chevron", packed(theme.filterIconColor)],
      ["funnel", packed(theme.filterIconActiveColor)],
      ["chevron", packed(theme.filterIconColor)],
    ]);
    expect(rectsIn(scene, packed(theme.selectionFill))).toHaveLength(1);
  });
});

describe("outlines", () => {
  test("of a spill are dashes as wide as whole pixels allow", () => {
    const scene = sceneOf(
      frameOf({ activeSpillRange: { firstRow: 1, lastRow: 3, firstCol: 1, lastCol: 2 } }),
    );
    const dashes = rectsIn(scene, packed(theme.spillBorderColor));
    const top = dashes.filter((rect) => rect.top === 50 && rect.height === 2);
    expect(top.length).toBe(Math.ceil((150 - 2) / 7));
    expect(top.slice(0, 3).map((rect) => [rect.left, rect.width])).toEqual([
      [120, 4],
      [127, 4],
      [134, 4],
    ]);
  });

  test("of a whole column are cut down to the screen without losing their place", () => {
    const rows = 1_000_000;
    const scene = sceneOf(
      frameOf({
        sheet: { rows, cols: 40 },
        scroll: { top: 12_345_678, left: 0 },
        selection: { row: 617_290, col: 1 },
        selectionRange: { firstRow: 0, endRow: rows, firstCol: 1, endCol: 2 },
        clipboardRange: { span: { firstRow: 0, endRow: rows, firstCol: 2, endCol: 3 }, cut: false },
      }),
    );
    const { width, height } = scene;
    const all = rectsOf(scene);
    expect(all.length).toBeLessThan(200);
    for (const rect of all) {
      expect(rect.left + rect.width).toBeLessThanOrEqual(width);
      expect(rect.top + rect.height).toBeLessThanOrEqual(height);
    }
    const dashes = rectsIn(scene, packed(theme.selection)).filter((rect) => rect.left === 268);
    // The right edge of the column starts where the top one has ended.
    const across = 75 - 2;
    for (const dash of dashes.slice(1)) {
      expect((across + 12_345_678 + dash.top - 30) % 6).toBe(0);
    }
    expect(dashes.length).toBeGreaterThan(20);
  });
});

describe("a selection", () => {
  const range = { firstRow: 1, endRow: 4, firstCol: 1, endCol: 3 };

  test("leaves the active cell the way it looks by itself", () => {
    const styles = [PLAIN, { ...PLAIN, fillColor: { kind: "palette" as const, slot: 4 } }];
    const cache = cacheOf({ "1:1": { text: "x", style: 1 } }, 1, 1);
    const frame = frameOf({ cache, styles, selection: { row: 1, col: 1 }, selectionRange: range });
    const fills = rectsOf(sceneOf(frame)).filter(
      (rect) => rect.left === 120 && rect.top === 50 && rect.width >= 75,
    );
    expect(fills.map((rect) => [rect.width, rect.height, rect.color])).toEqual([
      [75, 20, packed(theme.paletteFill[4] ?? "")],
      [150, 60, packed(theme.selectionFill)],
      [75, 20, packed(theme.paletteFill[4] ?? "")],
    ]);
    const missing = sceneOf({ ...frame, cache: frameOf().cache });
    expect(rectsIn(missing, packed(theme.placeholder)).at(-1)).toMatchObject({
      left: 120,
      top: 50,
      width: 75,
      height: 20,
    });
  });

  test("is outlined before its first row and column, and has a handle at its last", () => {
    const frame = frameOf({ selection: { row: 1, col: 1 }, selectionRange: range });
    const strokes = rectsIn(sceneOf(frame), packed(theme.selection));
    expect(strokes).toContainEqual({
      left: 118,
      top: 48,
      width: 152,
      height: 2,
      color: packed(theme.selection),
    });
    expect(strokes.at(-1)).toEqual({
      left: 267,
      top: 107,
      width: 6,
      height: 6,
      color: packed(theme.selection),
    });
    const without = rectsIn(sceneOf({ ...frame, showFillHandle: false }), packed(theme.selection));
    expect(without).toHaveLength(strokes.length - 1);
  });

  test("that ends at the freeze line has its whole handle, over the line and what is past it", () => {
    const frame = frameOf({
      ...FROZEN,
      cache: cacheOf({ "0:0": "frozen" }, 1, 1),
      selection: { row: 0, col: 0 },
      selectionRange: { firstRow: 0, endRow: 1, firstCol: 0, endCol: 1 },
    });
    const painted = rectsOf(sceneOf(frame));
    const handle = { left: 117, top: 47, width: 6, height: 6, color: packed(theme.selection) };
    expect(painted.at(-1)).toEqual(handle);
    expect(painted.findLastIndex(({ color }) => color === packed(theme.freezeLine))).toBeLessThan(
      painted.length - 2,
    );
    // Not over the rows that are frozen, though, when it is below them.
    const below = { row: 1, col: 0 };
    const scrolled = sceneOf({
      ...frame,
      scroll: { top: 19, left: 5 },
      selection: below,
      selectionRange: { firstRow: 1, endRow: 2, firstCol: 0, endCol: 1 },
    });
    expect(rectsOf(scrolled).at(-1)).toEqual({ ...handle, top: 50, height: 4 });
  });

  test("marks the headers of all its ranges", () => {
    const ranges = [range, { firstRow: 5, endRow: 6, firstCol: 0, endCol: 1 }];
    const scene = sceneOf(
      frameOf({
        selection: { row: 5, col: 0 },
        selectionRange: ranges[1] ?? null,
        selectionRanges: ranges,
      }),
    );
    const marked = rectsIn(scene, packed(theme.headerSelected));
    expect(marked).toEqual([
      expect.objectContaining({ left: 120, top: 0, width: 150 }),
      expect.objectContaining({ left: 45, top: 0, width: 75 }),
      expect.objectContaining({ left: 0, top: 50, height: 60 }),
      expect.objectContaining({ left: 0, top: 130, height: 20 }),
    ]);
  });
});

describe("a diff", () => {
  class CountedMap extends Map<string, "added" | "conflict" | "deleted"> {
    walks = 0;
    override [Symbol.iterator]() {
      this.walks++;
      return super[Symbol.iterator]();
    }
  }

  test("is read once, however many frames and panes it is drawn in", () => {
    const map = new CountedMap([
      ["5:2", "added"],
      ["0:0", "conflict"],
      ["2:1", "deleted"],
      ["39:39", "added"],
      ["nonsense", "added"],
    ]);
    const builder = new SceneBuilder(measure, colors);
    const frame = frameOf({ ...FROZEN, diffHighlights: map as DiffHighlightMap });
    for (let round = 0; round < 3; round++) {
      sceneOf(frame, builder);
    }
    expect(map.walks).toBe(1);
    map.set("3:3", "deleted");
    const scene = sceneOf(frame, builder);
    expect(map.walks).toBe(2);
    expect(rectsIn(scene, FIXED.addedFill)).toHaveLength(1);
    expect(rectsIn(scene, FIXED.conflictFill)).toHaveLength(1);
    expect(rectsIn(scene, FIXED.deletedFill)).toHaveLength(2);
    // Four sides and a line through the middle of each; one's left side is frozen over.
    expect(rectsIn(scene, FIXED.deleted)).toHaveLength(9);
  });
});

describe("headers", () => {
  test("keep their labels inside their cells", () => {
    const layout = {
      ...axes(),
      rowAxis: new SparseAxis(20, [[2, 6]]),
      colAxis: new SparseAxis(75, [[1, 7]]),
    };
    const named = new Map(
      labels(sceneOf(frameOf({ layout, scroll: { top: 5, left: 70 } }))).map((label) => [
        label.text,
        label,
      ]),
    );
    expect(named.get("A")?.clip).toEqual([45, 0, 49, 29]);
    expect(named.get("B")?.clip).toEqual([50, 0, 56, 29]);
    expect(named.get("C")?.clip).toBeNull();
    expect(named.get("3")?.clip).toEqual([0, 65, 44, 70]);
    expect(named.get("2")?.clip).toBeNull();
  });

  test("mark the corner when everything is selected", () => {
    const all = { firstRow: 0, endRow: 40, firstCol: 0, endCol: 40 };
    const scene = sceneOf(frameOf({ selection: { row: 0, col: 0 }, selectionRange: all }));
    expect(rectsIn(scene, packed(theme.headerSelected)).at(-1)).toMatchObject({
      left: 0,
      top: 0,
      width: 45,
      height: 30,
    });
    expect(sprites(scene).at(-1)).toMatchObject({
      x: 32,
      y: 17,
      spec: { kind: "corner", width: 8 },
    });
  });

  test("have dividers on the freeze lines and a guide where rows would be dropped", () => {
    const frozen = sceneOf(frameOf({ layout: axes([], [], 2, 1) }));
    expect(rectsIn(frozen, packed(theme.freezeLine))).toEqual([
      expect.objectContaining({ left: 0, top: 69, width: 340, height: 2 }),
      expect.objectContaining({ left: 119, top: 0, width: 2, height: 180 }),
    ]);
    const scroll = { top: 60, left: 0 };
    const guide = sceneOf(frameOf({ scroll, reorderGuide: { axis: "row", beforeRow: 5 } }));
    expect(rectsIn(guide, packed(theme.selection))).toEqual([
      expect.objectContaining({ left: 0, top: 69, width: 340, height: 3 }),
    ]);
  });
});

describe("what is counted", () => {
  test("is the cells in view with text, and those whose page has not arrived", () => {
    const cells: Record<string, string> = {};
    for (let row = 0; row < 64; row++) {
      cells[`${row}:0`] = "x";
      cells[`${row}:20`] = "y";
    }
    const scroll = { top: 45, left: 13.5 * 75 };
    const scene = sceneOf(frameOf({ cache: cacheOf(cells, 1, 2), scroll }));
    expect([scene.cells, scene.missing]).toEqual([0, 0]);
    const home = sceneOf(frameOf({ cache: cacheOf(cells, 1, 2), scroll: { top: 45, left: 0 } }));
    expect([home.cells, home.missing]).toEqual([8, 0]);
    const partly = sceneOf(frameOf({ cache: cacheOf(cells, 1, 2, ["0:1"]), scroll }));
    expect(partly.missing).toBe(8 * 2);
  });
});

describe("at any density", () => {
  const busy: Partial<Frame> = {
    cache: cacheOf({ "1:1": { text: "12", flags: FLAG_NUMBER }, "2:0": "text" }, 1, 1),
    selection: { row: 1, col: 1 },
    selectionRange: { firstRow: 1, endRow: 5, firstCol: 1, endCol: 3 },
    formulaReferences: [{ colorIndex: 2, firstRow: 0, lastRow: 3, firstCol: 2, lastCol: 3 }],
    activeSpillRange: { firstRow: 4, lastRow: 5, firstCol: 0, lastCol: 1 },
    fillPreview: { firstRow: 1, endRow: 6, firstCol: 1, endCol: 3 },
    diffHighlights: new Map([["3:2", "deleted"]]),
    reorderGuide: { axis: "col", beforeCol: 2 },
    filterState: {
      headerRow: 0,
      lastRow: 10,
      firstCol: 0,
      lastCol: 4,
      activeCols: [],
      hiddenRowSpans: [],
    },
    remotePresences: [{ replica: 1, displayName: "Li", colorSlot: 1, cursorRow: 4, cursorCol: 3 }],
    layout: axes([], [], 1, 1),
  };

  test("every edge is on a whole device pixel inside the frame", () => {
    for (const scale of [1, 1.1, 1.25, 1.5, 1.75, 2, 3]) {
      const aligned = (css: number) => Math.round(css * scale) / scale;
      const scroll = { top: aligned(33), left: aligned(17) };
      const scene = sceneOf(frameOf({ ...busy, scale, scroll }));
      expect(scene.width).toBe(Math.round(350 * scale));
      expect(scene.height).toBe(Math.round(190 * scale));
      for (const rect of rectsOf(scene)) {
        expect(Object.values(rect).every(Number.isInteger)).toBe(true);
        expect(rect.left).toBeGreaterThanOrEqual(0);
        expect(rect.top).toBeGreaterThanOrEqual(0);
        expect(rect.left + rect.width).toBeLessThanOrEqual(scene.width);
        expect(rect.top + rect.height).toBeLessThanOrEqual(scene.height);
      }
      for (const step of scene.steps) {
        if ("x" in step) {
          expect(Number.isInteger(step.x) && Number.isInteger(step.y)).toBe(true);
        }
      }
    }
  });

  test("nothing is painted outside a frame smaller than its headers", () => {
    for (const size of [
      { width: 30, height: 20 },
      { width: 350, height: 12 },
      { width: 20, height: 190 },
    ]) {
      for (const rect of rectsOf(sceneOf(frameOf({ ...busy, size })))) {
        expect(rect.left + rect.width).toBeLessThanOrEqual(size.width);
        expect(rect.top + rect.height).toBeLessThanOrEqual(size.height);
      }
    }
  });

  test("the frame is painted opaque before anything else", () => {
    const scene = sceneOf(frameOf({ scale: 1.5 }));
    expect(rectsOf(scene)[0]).toEqual({
      left: 0,
      top: 0,
      width: 525,
      height: 285,
      color: packColor(255, 255, 255),
    });
  });

  test("grid lines are the last pixels before the edges of cells", () => {
    for (const [scale, width] of [
      [1, 1],
      [1.25, 1],
      [1.5, 2],
      [2, 2],
    ] as const) {
      const lines = rectsIn(sceneOf(frameOf({ scale })), packed(theme.line));
      const down = lines.filter((rect) => rect.width === width && rect.height > 20 * scale);
      const first = Math.round(45 * scale) + Math.round(75 * scale);
      expect(down[0]?.left).toBe(first - width);
    }
  });
});

type Sizes = readonly (readonly [number, number])[];

/** A frame the way the grid has it at a zoom: the sizes in it are already those on the screen. */
function zoomedFrame(
  zoom: number,
  changes: Partial<Frame> = {},
  { scale = 1, rows = [] as Sizes, cols = [] as Sizes, frozen = 0 } = {},
): Frame {
  return frameOf({
    zoom,
    scale,
    metrics: viewMetrics(METRICS, zoom, scale),
    layout: {
      rowAxis: new SparseAxis(METRICS.rowHeight, rows, scale, zoom),
      colAxis: new SparseAxis(METRICS.colWidth, cols, scale, zoom),
      frozenRows: frozen,
      frozenCols: frozen,
    },
    sheet: { rows: 1000, cols: 400 },
    size: { width: 640, height: 400 },
    ...changes,
  });
}

/** The labels of rows and their middles, from the top. */
function numbersOf(scene: Scene): { readonly number: number; readonly middle: number }[] {
  const step = scene.steps.find((candidate) => candidate.kind === "labels");
  if (step?.kind !== "labels") {
    throw new Error("no labels");
  }
  const { runs } = step;
  return runs.texts
    .slice(0, runs.length)
    .flatMap((text, run) =>
      /^\d+$/.test(text) ? [{ number: Number(text), middle: runs.baseline[run] ?? 0 }] : [],
    )
    .sort((a, b) => a.middle - b.middle);
}

describe("at a zoom", () => {
  test("the cells' text grows, the headers' within limits, and the name plates' not", () => {
    const presence = { replica: 1, displayName: "Li", colorSlot: 1, cursorRow: 4, cursorCol: 1 };
    for (const [zoom, cells, headers] of [
      [0.25, 3.25, 9.75],
      [1, 13, 13],
      [1.25, 16.25, 16.25],
      [2, 26, 19.5],
    ] as const) {
      const scene = sceneOf(zoomedFrame(zoom, { remotePresences: [presence] }));
      expect(scene.look.fonts).toEqual([
        `${cells}px sans-serif`,
        `700 ${cells}px sans-serif`,
        `italic ${cells}px sans-serif`,
        `italic 700 ${cells}px sans-serif`,
        `${headers}px sans-serif`,
      ]);
      const step = scene.steps.find((candidate) => candidate.kind === "labels");
      expect(
        step?.kind === "labels" && new Set(step.runs.font.subarray(0, step.runs.length)),
      ).toEqual(new Set([4]));
      const plate = sprites(scene).find((sprite) => sprite.spec.kind === "plate");
      expect(plate?.spec).toMatchObject({ height: 18, font: "600 11px sans-serif", padding: 6 });
    }
  });

  test("lines are as wide as ever", () => {
    for (const zoom of ZOOM_STEPS) {
      const scene = sceneOf(zoomedFrame(zoom, { selection: { row: 3, col: 2 } }, { frozen: 1 }));
      const { metrics } = zoomedFrame(zoom);
      for (const line of rectsIn(scene, packed(theme.line))) {
        expect(Math.min(line.width, line.height)).toBe(1);
      }
      for (const line of rectsIn(scene, packed(theme.freezeLine))) {
        expect(Math.min(line.width, line.height)).toBe(2);
      }
      const outline = rectsIn(scene, packed(theme.selection));
      if (metrics.rowHeight > 8) {
        expect(outline).toHaveLength(4);
        expect(outline.every((side) => Math.min(side.width, side.height) === 2)).toBe(true);
      }
    }
  });

  test("the fill handle is no larger than half of its cell", () => {
    const range = { firstRow: 1, endRow: 4, firstCol: 1, endCol: 3 };
    for (const [zoom, size] of [
      [0.25, 2],
      [0.5, 5],
      [1, 6],
      [2, 6],
    ] as const) {
      const frame = zoomedFrame(zoom, { selection: { row: 1, col: 1 }, selectionRange: range });
      expect(rectsIn(sceneOf(frame), packed(theme.selection)).at(-1)).toMatchObject({
        width: size,
        height: size,
      });
    }
  });

  test("filter buttons grow with their cells", () => {
    const filterState = {
      headerRow: 0,
      lastRow: 10,
      firstCol: 0,
      lastCol: 0,
      activeCols: [0],
      hiddenRowSpans: [],
    };
    for (const [zoom, size] of [
      [0.5, 7],
      [1, 14],
      [2, 28],
    ] as const) {
      const scene = sceneOf(zoomedFrame(zoom, { filterState }));
      const funnel = sprites(scene).find((step) => step.spec.kind === "funnel");
      expect(funnel?.spec).toMatchObject({ width: size, height: size, scale: zoom });
      const { metrics } = zoomedFrame(zoom);
      const right = metrics.headerWidth + metrics.colWidth;
      expect(Math.abs(right - (funnel?.x ?? 0) - size - 3 * zoom)).toBeLessThanOrEqual(0.5);
    }
  });
});

describe("labels of headers that are too small for them", () => {
  // Twice what the baseline is below the middle, and a line above and below.
  const needed = Math.ceil(9.75 * 0.83) + 2;

  test("are those of every second, fifth or tenth one", () => {
    const steps = ZOOM_STEPS.map((zoom) => {
      const { rows, frame } = {
        rows: numbersOf(sceneOf(zoomedFrame(zoom))).map(({ number }) => number),
        frame: zoomedFrame(zoom),
      };
      const step = (rows[1] ?? 0) - (rows[0] ?? 0);
      expect(rows.slice(1, -1).every((number) => number % step === 0)).toBe(true);
      expect(step * frame.metrics.rowHeight).toBeGreaterThanOrEqual(needed);
      return step;
    });
    expect(steps.slice(0, 4)).toEqual([5, 2, 2, 1]);
    expect(new Set(steps.slice(3))).toEqual(new Set([1]));
  });

  test("of columns too, with the longest name in view deciding", () => {
    const base = { ...METRICS, colWidth: 48 };
    const names = (firstCol: number, width: number) => {
      const frame = zoomedFrame(0.25);
      const layout = { ...frame.layout, colAxis: new SparseAxis(48, [], 1, 0.25) } as SheetLayout;
      // A column is 12 px wide.
      const scene = sceneOf(
        zoomedFrame(0.25, {
          metrics: viewMetrics(base, 0.25, 1),
          layout,
          size: { width, height: 400 },
          scroll: { top: 0, left: firstCol * 12 },
        }),
      );
      return labels(scene)
        .map(({ text }) => text)
        .filter((text) => /^[A-Z]+$/.test(text));
    };
    expect(names(0, 34 + 10 + 10 * 12).slice(0, 4)).toEqual(["A", "B", "C", "D"]);
    const wide = names(0, 640);
    expect(wide.slice(0, 4)).toEqual(["A", "B", "C", "D"]);
    expect(wide.slice(24, 29)).toEqual(["Y", "Z", "AA", "AC", "AE"]);
    expect(names(60, 640)).toContain("BI");
    expect(names(60, 640)).not.toContain("BJ");
  });

  test("are the same ones wherever the sheet is scrolled to", () => {
    const at = (top: number) => numbersOf(sceneOf(zoomedFrame(0.25, { scroll: { top, left: 0 } })));
    const before = new Map(at(500).map(({ number, middle }) => [number, middle]));
    let compared = 0;
    for (let by = 1; by < 30; by++) {
      for (const { number, middle } of at(500 + by)) {
        const was = before.get(number);
        if (was !== undefined) {
          expect(was - middle).toBe(by);
          compared++;
        }
      }
    }
    expect(compared).toBeGreaterThan(200);
  });

  test("come into view before their rows do", () => {
    // Row 5 is 5 px high and ends where the cells begin: the lower half of its label is in view.
    const scene = sceneOf(zoomedFrame(0.25, { scroll: { top: 25, left: 0 } }));
    const [first] = numbersOf(scene);
    expect(first?.number).toBe(5);
    const { clip } = labels(scene).find((label) => label.text === "5") ?? {};
    expect(clip?.[1]).toBe(Math.round(30 * 0.75));
  });

  test("leave room for those of headers that are large enough", () => {
    const rows: Sizes = [
      [11, 80],
      [12, 80],
      [13, 18],
      [20, 60],
      [33, 2],
      [34, 2],
      [40, 0],
    ];
    const scene = sceneOf(zoomedFrame(0.25, {}, { rows }));
    const shown = numbersOf(scene);
    const numbers = shown.map(({ number }) => number);
    for (const large of [12, 13, 21]) {
      expect(numbers).toContain(large);
    }
    expect(numbers).not.toContain(41);
    for (let index = 1; index < shown.length; index++) {
      const distance = (shown[index]?.middle ?? 0) - (shown[index - 1]?.middle ?? 0);
      expect(distance, `${numbers[index]}`).toBeGreaterThanOrEqual(needed - 1);
    }
  });

  test("have marks between them instead of lines", () => {
    const lines = (zoom: number) =>
      rectsIn(sceneOf(zoomedFrame(zoom)), packed(theme.headerLine)).filter(
        (rect) => rect.height === 1 && rect.top > 60,
      );
    expect(new Set(lines(0.25).map((rect) => rect.width))).toEqual(new Set([4]));
    const { metrics } = zoomedFrame(1);
    expect(new Set(lines(1).map((rect) => rect.width))).toEqual(new Set([metrics.headerWidth]));
  });

  test("are those of the rows that are left by a filter", () => {
    const filterRowMap = new FilterRowMap(1000, [[10, 899]]);
    const filterState = {
      headerRow: 0,
      lastRow: 950,
      firstCol: 0,
      lastCol: 2,
      activeCols: [1],
      hiddenRowSpans: [[10, 899]] as const,
    };
    const frame = zoomedFrame(0.25, {
      filterRowMap,
      filterState,
      sheet: { rows: filterRowMap.visibleRows, cols: 400 },
    });
    expect(
      numbersOf(sceneOf(frame))
        .map(({ number }) => number)
        .slice(0, 4),
    ).toEqual([5, 10, 905, 910]);
  });
});

describe("between frames", () => {
  test("a page is not laid out again because the sheet was scrolled", () => {
    const builder = new SceneBuilder(measure, colors);
    const cache = cacheOf({ "1:1": "a", "30:3": "b" }, 1, 1);
    const layoutsAt = (top: number) =>
      sceneOf(frameOf({ cache, scroll: { top, left: 0 } }), builder).steps.flatMap((step) =>
        step.kind === "texts" ? [step.layout] : [],
      );
    const [first] = layoutsAt(0);
    expect(layoutsAt(333)).toEqual([first]);
    expect(layoutsAt(333)[0]).toBe(first);
    builder.reset();
    expect(layoutsAt(333)[0]).not.toBe(first);
  });
});
