import { describe, expect, test } from "vitest";
import { NamedColumns } from "./columns.ts";
import { SparseAxis, viewMetrics } from "./layout.ts";
import {
  axes,
  colors,
  frameOf,
  METRICS,
  measure,
  rectsOf,
  sceneOf,
  theme,
} from "./render/fixtures.ts";
import type { Frame } from "./render/frame.ts";
import type { Scene } from "./render/scene.ts";
import { SceneBuilder } from "./render/scene.ts";

const HEADER = "13px sans-serif";
const ELLIPSIS = "\u{2026}";
const widthOf = (text: string, font = HEADER) => measure.width(font, text);
const packed = (css: string) => colors.parse(css);

interface Label {
  readonly text: string;
  readonly x: number;
  readonly clip: readonly number[] | null;
}

function labelsOf(scene: Scene): Label[] {
  const step = scene.steps.find((candidate) => candidate.kind === "labels");
  if (step?.kind !== "labels") {
    throw new Error("no labels");
  }
  const { runs } = step;
  return runs.texts.slice(0, runs.length).map((text, run) => ({
    text,
    x: runs.x[run] ?? 0,
    clip: runs.clipped[run] === 1 ? [...runs.clip.subarray(run * 4, run * 4 + 4)] : null,
  }));
}

/** The labels that are no numbers of rows. */
const namesOf = (scene: Scene) => labelsOf(scene).filter(({ x }) => x >= METRICS.headerWidth);
const spritesOf = (scene: Scene) =>
  scene.steps.flatMap((step) =>
    step.kind === "sprite" && step.spec.kind !== "corner" ? [step] : [],
  );

/** Columns of 75 by 30 pixels, from 45 on. */
const named = (columns: NamedColumns | null, changes: Partial<Frame> = {}) =>
  sceneOf(frameOf({ columns, sheet: { rows: 40, cols: 3 }, ...changes }));

describe("the header of a column with a name", () => {
  test("has the name at its left, and a button at its right", () => {
    const scene = named(new NamedColumns([{ name: "id" }, { name: "to" }, { name: "" }]));
    expect(namesOf(scene)).toEqual([
      { text: "id", x: 45 + 6, clip: null },
      { text: "to", x: 120 + 6, clip: null },
    ]);
    expect(spritesOf(scene).map(({ x, y, spec }) => [x, y, spec.kind, spec.width])).toEqual(
      [45, 120, 195].map((left) => [left + 74 - 5 - 16, 7, "chevron", 16]),
    );
  });

  test("has a name as long as there is room, and says when that is not all of it", () => {
    const name = "quantity of the goods ordered";
    const [label] = namesOf(named(new NamedColumns([{ name }])));
    const text = label?.text ?? "";
    expect(text.endsWith(ELLIPSIS)).toBe(true);
    const kept = text.slice(0, -1);
    expect(name.startsWith(kept)).toBe(true);
    const room = 74 - 5 - 16 - 3 - 6;
    expect(widthOf(text)).toBeLessThanOrEqual(room);
    expect(widthOf(name.slice(0, kept.length + 2) + ELLIPSIS)).toBeGreaterThan(room);
  });

  test("has no half of a character, and nothing at all where there is no room for any", () => {
    for (let length = 1; length < 12; length++) {
      const name = "x".repeat(length) + "\u{1f600}".repeat(20);
      const [label] = namesOf(named(new NamedColumns([{ name }])));
      expect(label?.text.isWellFormed()).toBe(true);
    }
    const narrow = { ...axes(), colAxis: new SparseAxis(75, [[0, 24]]) };
    const scene = named(new NamedColumns([{ name: "narrow" }, { name: "b" }]), { layout: narrow });
    const [first] = namesOf(scene);
    expect(first?.text).toMatch(/^n?\u{2026}$/u);
    const least = { ...axes(), colAxis: new SparseAxis(75, [[0, 12]]) };
    const none = named(new NamedColumns([{ name: "narrow" }, { name: "b" }]), { layout: least });
    expect(namesOf(none).map(({ text }) => text)).toEqual(["b"]);
    expect(spritesOf(none)).toHaveLength(2);
  });

  test("shows which way the rows are sorted, and by how many columns before", () => {
    const columns = new NamedColumns([
      { name: "a", sort: "ascending", sortOrder: 1 },
      { name: "b", sort: "descending", sortOrder: 12 },
      { name: "c", sort: "ascending" },
    ]);
    const scene = named(columns);
    const arrows = spritesOf(scene).filter(({ spec }) => spec.kind === "shape");
    expect(arrows).toHaveLength(3);
    const tips = arrows.map(({ spec }) =>
      spec.kind === "shape" ? Math.sign(spec.corners[0]?.[1] ?? 0) : 0,
    );
    expect(tips).toEqual([-1, 1, -1]);
    const orders = namesOf(scene).filter(({ text }) => /\d/.test(text));
    expect(orders.map(({ text }) => text)).toEqual(["1", "12"]);
    // The order is right after its arrow, and before the button.
    const button = 45 + 74 - 5 - 16 - 3;
    expect(orders[0]?.x).toBe((arrows[0]?.x ?? 0) + 10);
    expect((orders[0]?.x ?? 0) + widthOf("1")).toBeLessThanOrEqual(button);
    expect(arrows[2]?.x).toBe(45 + 150 + 74 - 5 - 16 - 3 - 10);
    expect(columns.sortOf(1)).toBe("descending");
    expect(columns.sortOf(7)).toBeNull();
  });

  test("leaves the arrow out before the button, and the name before the arrow", () => {
    const columns = new NamedColumns([{ name: "wide", sort: "ascending", sortOrder: 3 }]);
    const at = (width: number) => {
      const layout = { ...axes(), colAxis: new SparseAxis(75, [[0, width]]) };
      const scene = named(columns, { layout, sheet: { rows: 40, cols: 1 } });
      return [
        ...spritesOf(scene).map(({ spec }) => spec.kind),
        ...namesOf(scene).map(({ text }) => text),
      ];
    };
    expect(at(120)).toEqual(["chevron", "shape", "3", "wide"]);
    expect(at(46)).toEqual(["chevron", "shape", "3"]);
    expect(at(44)).toEqual(["shape", "3", `w${ELLIPSIS}`]);
    expect(at(24)).toEqual(["shape", "3"]);
    expect(at(18)).toEqual([ELLIPSIS]);
    expect(at(8)).toEqual([]);
  });

  test("has a funnel on a ground of its own while rows are filtered by it", () => {
    const scene = named(new NamedColumns([{ name: "a", filtered: true }, { name: "b" }]));
    const signs = spritesOf(scene);
    expect(signs.map(({ spec }) => [spec.kind, spec.color])).toEqual([
      ["funnel", packed(theme.filterIconActiveColor)],
      ["chevron", packed(theme.filterIconColor)],
      ["chevron", packed(theme.filterIconColor)],
    ]);
    const grounds = rectsOf(scene).filter((rect) => rect.color === packed(theme.selectionFill));
    expect(grounds).toEqual([expect.objectContaining({ left: 98, top: 7, width: 16, height: 16 })]);
  });

  test("is cut off where it is scrolled off, and is not under the frozen ones", () => {
    const columns = new NamedColumns(
      Array.from({ length: 12 }, (_, col) => ({ name: `column ${col}`, sort: "ascending" })),
    );
    const scene = sceneOf(
      frameOf({
        columns,
        sheet: { rows: 40, cols: 12 },
        layout: axes([], [], 0, 1),
        scroll: { top: 0, left: 100 },
      }),
    );
    const names = labelsOf(scene).filter(({ text }) => text.startsWith("c"));
    expect(names.map(({ text }) => text[0])).toEqual(Array(names.length).fill("c"));
    // The frozen column ends at 120, and the second one in view began at 95.
    const [, under] = names;
    expect(under?.x).toBe(45 + 150 - 100 + 6);
    expect(under?.clip?.[0]).toBe(120);
    const sprites = spritesOf(scene);
    expect(sprites.filter(({ clip }) => clip.left === 120).length).toBeGreaterThan(2);
    expect(sprites.every(({ clip }) => clip.bottom === 30)).toBe(true);
  });

  test("is marked like any other when it is selected", () => {
    const selection = { row: 2, col: 1 };
    const scene = named(new NamedColumns([{ name: "a" }, { name: "b" }]), { selection });
    const marks = rectsOf(scene).filter((rect) => rect.color === packed(theme.headerSelected));
    expect(marks[0]).toMatchObject({ left: 120, top: 0, width: 75 });
  });

  test("is there when there are no rows", () => {
    const scene = sceneOf(
      frameOf({ columns: new NamedColumns([{ name: "a" }]), sheet: { rows: 0, cols: 1 } }),
    );
    expect(labelsOf(scene).map(({ text }) => text)).toEqual(["a"]);
    expect(spritesOf(scene)).toHaveLength(1);
    expect([scene.cells, scene.missing]).toEqual([0, 0]);
  });
});

describe("at a zoom and a density", () => {
  const columns = new NamedColumns(
    Array.from({ length: 40 }, (_, col) => ({
      name: `name ${col} of a column`,
      sort: col % 3 === 0 ? ("descending" as const) : null,
      sortOrder: col % 6 === 0 ? col : undefined,
      filtered: col % 2 === 0,
    })),
  );

  test("every column has its name, and everything is on whole pixels inside its header", () => {
    for (const zoom of [0.25, 0.5, 0.75, 1, 1.25, 1.5, 2]) {
      for (const scale of [1, 1.1, 1.25, 1.5, 2]) {
        const metrics = viewMetrics({ ...METRICS, headerHeight: 24 }, zoom, scale);
        const snapped = (css: number) => Math.round(css * scale) / scale;
        const frame = frameOf({
          columns,
          zoom,
          scale,
          metrics,
          layout: {
            rowAxis: new SparseAxis(METRICS.rowHeight, [], scale, zoom),
            colAxis: new SparseAxis(112, [], scale, zoom),
            frozenRows: 0,
            frozenCols: 1,
          },
          sheet: { rows: 1000, cols: 40 },
          scroll: { top: 0, left: snapped(37) },
          size: { width: 640, height: 400 },
        });
        const scene = sceneOf(frame);
        const setting = `zoom ${zoom}, scale ${scale}`;
        const bottom = Math.round(metrics.headerHeight * scale);
        const names = labelsOf(scene).filter(({ text }) => text.startsWith("n"));
        // Only arrows leave no room for them, at the smallest zoom.
        const { firstCol, endCol } = frame.visible;
        const least = (endCol - firstCol - 1) * (zoom < 0.5 ? 0.6 : 1);
        expect(names.length, setting).toBeGreaterThanOrEqual(Math.floor(least));
        for (const { x, y, spec } of spritesOf(scene)) {
          expect([x, y, spec.width].every(Number.isInteger), setting).toBe(true);
          expect(y, setting).toBeGreaterThanOrEqual(0);
          expect(y + spec.height, setting).toBeLessThanOrEqual(bottom);
        }
        for (const rect of rectsOf(scene)) {
          expect(Object.values(rect).every(Number.isInteger), setting).toBe(true);
        }
      }
    }
  });

  test("the button is where it is drawn", () => {
    for (const [zoom, scale] of [
      [0.75, 1],
      [1, 1.5],
      [1.5, 2],
    ] as const) {
      const button = columns.buttonIn(100, 112, 24 * zoom, zoom);
      expect(button).not.toBeNull();
      const { x = 0, y = 0, size = 0 } = button ?? {};
      expect(x + size).toBeCloseTo(212 - 5 * zoom, 9);
      expect(y + size / 2).toBeCloseTo(12 * zoom, 9);
      // In device pixels it is the same one.
      const device = columns.buttonIn(100 * scale, 112 * scale, 24 * zoom * scale, zoom * scale);
      expect((device?.x ?? 0) / scale).toBeCloseTo(x, 9);
    }
    expect(columns.buttonIn(0, 44, 24, 1)).toBeNull();
    expect(columns.buttonIn(0, 45, 24, 1)).not.toBeNull();
    expect(columns.buttonIn(0, 100, 12, 1)).toBeNull();
  });
});

test("a name is shortened once for each size of its column", () => {
  const name = "a name that is far too long for its column";
  const columns = new NamedColumns([{ name }]);
  let measured = 0;
  const counting = {
    ...measure,
    width: (font: string, text: string) => {
      measured += text.length > 1 && name.startsWith(text) && text !== name ? 1 : 0;
      return measure.width(font, text);
    },
  };
  const builder = new SceneBuilder(counting, colors);
  const frame = frameOf({ columns, sheet: { rows: 40, cols: 1 } });
  builder.build(frame);
  expect(measured).toBeGreaterThan(0);
  const first = measured;
  builder.build(frame);
  expect(measured).toBe(first);
  builder.build({ ...frame, layout: axes([], [[0, 90]]) });
  expect(measured).toBeGreaterThan(first);
});

test("a name of any length is measured no further than it could be shown", () => {
  let longest = 0;
  const counting = {
    ...measure,
    width: (font: string, text: string) => {
      longest = Math.max(longest, text.length);
      return measure.width(font, text);
    },
  };
  const columns = new NamedColumns([{ name: "x".repeat(1_000_000) }]);
  new SceneBuilder(counting, colors).build(frameOf({ columns, sheet: { rows: 40, cols: 1 } }));
  expect(longest).toBe(512);
});

test("a column needs room for its button, its arrow and its order besides its name", () => {
  const columns = new NamedColumns([
    { name: "a" },
    { name: "a", sort: "ascending" },
    { name: "a", sort: "ascending", sortOrder: 2 },
  ]);
  const [plain = 0, sorted = 0, ordered = 0] = [0, 1, 2].map((col) => columns.roomOf(col));
  expect(plain).toBeGreaterThan(0);
  expect(sorted).toBeGreaterThan(plain);
  expect(ordered).toBeGreaterThan(sorted);
  expect(columns.nameOf(3)).toBe("");
  // A header as wide as its name and that much has all of its name.
  for (const [col, room] of [plain, sorted, ordered].entries()) {
    const name = "name of a column";
    const fit = new NamedColumns([
      {
        ...[{}, { sort: "ascending" as const }, { sort: "ascending" as const, sortOrder: 2 }][col],
        name,
      },
    ]);
    const width = Math.ceil(widthOf(name, `600 ${HEADER}`) + 24 + room);
    const scene = sceneOf(
      frameOf({
        columns: fit,
        sheet: { rows: 40, cols: 1 },
        layout: axes([], [[0, width]]),
        size: { width: 600, height: 190 },
      }),
    );
    expect(labelsOf(scene).map(({ text }) => text)).toContain(name);
  }
});
