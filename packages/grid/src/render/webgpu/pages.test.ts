import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { SparseAxis } from "../../layout.ts";
import { axes, cacheOf, colors, frameOf, METRICS, measure, PLAIN, pageOf } from "../fixtures.ts";
import type { Frame } from "../frame.ts";
import { bandsOf, type PageStep, type Scene, SceneBuilder } from "../scene.ts";
import { Atlas } from "./atlas.ts";
import { FakeCanvas, FakeDevice, fakeBrowser, quadsOf } from "./fakes.ts";
import { Glyphs } from "./glyphs.ts";
import { RetainedPages, Stretches } from "./pages.ts";
import type { Raster } from "./raster.ts";

beforeEach(() => {
  fakeBrowser();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("stretches", () => {
  test("are given out from the first place there is, and join when they come back", () => {
    const stretches = new Stretches(100);
    expect([stretches.take(30), stretches.take(30), stretches.take(40)]).toEqual([0, 30, 60]);
    expect(stretches.take(1)).toBeNull();
    stretches.give(0, 30);
    stretches.give(60, 40);
    expect(stretches.take(35)).toBe(60);
    expect(stretches.take(31)).toBeNull();
    stretches.give(30, 30);
    expect(stretches.take(60)).toBe(0);
    stretches.give(60, 35);
    stretches.give(0, 60);
    expect(stretches.isEmpty).toBe(true);
    expect(stretches.take(100)).toBe(0);
  });

  test("never overlap", () => {
    const stretches = new Stretches(1000);
    const taken: [number, number][] = [];
    let state = 3;
    const next = (bound: number) => {
      state = (state * 1103515245 + 12345) % 2 ** 31;
      return state % bound;
    };
    for (let step = 0; step < 2000; step++) {
      if (next(5) < 3) {
        const length = 1 + next(80);
        const start = stretches.take(length);
        if (start !== null) {
          for (const [other, size] of taken) {
            expect(start + length <= other || other + size <= start).toBe(true);
          }
          expect(start + length).toBeLessThanOrEqual(1000);
          taken.push([start, length]);
        }
      } else if (taken.length > 0) {
        const [stretch] = taken.splice(next(taken.length), 1);
        stretches.give(...(stretch ?? [0, 0]));
      }
    }
    for (const [start, length] of taken) {
      stretches.give(start, length);
    }
    expect(stretches.isEmpty).toBe(true);
  });
});

function open() {
  const device = new FakeDevice();
  const canvas = new FakeCanvas();
  const raster = {
    kind: "copy" as const,
    context: canvas.context as unknown as CanvasRenderingContext2D,
    clear: () => {},
    upload: vi.fn<Raster["upload"]>(),
  };
  const guard = <T>(allocate: () => T) => allocate();
  const glyphs = new Glyphs(
    (onEvict) => new Atlas(device.asDevice, raster, guard, onEvict),
    () => ({ phases: 4, ascii: true }),
  );
  const pages = new RetainedPages(device.asDevice, glyphs, guard);
  const scenes = new SceneBuilder(measure, colors);
  /** Makes everything a frame shows, as drawing it would. */
  const show = (frame: Frame, budget = Number.POSITIVE_INFINITY): Scene => {
    const scene = scenes.build(frame);
    pages.begin(frame.cache);
    glyphs.begin(budget);
    glyphs.atlas.plan.now++;
    for (const step of scene.steps) {
      if (step.kind === "fills" || step.kind === "borders") {
        pages.rects(scene.look, step.layout, step.kind);
      } else if (step.kind === "texts") {
        const { first, end } = bandsOf(step);
        for (let index = first; index < end; index++) {
          pages.make(scene.look, step.layout, index, viewOf(step));
        }
      }
    }
    pages.flush();
    return scene;
  };
  const uploads = () => device.recorder.named("writeBuffer");
  /** The first group of rows of the first page that a scene shows. */
  const bandOf = ({ steps, look }: Scene) => {
    const step = steps.find((step) => step.kind === "texts");
    if (step?.kind !== "texts") {
      throw new Error("no texts");
    }
    const view = viewOf(step);
    return {
      state: pages.stateOf(look, step.layout, 0, view),
      shown: pages.shown(look, step.layout, 0, view),
    };
  };
  return { device, glyphs, pages, show, uploads, scenes, bandOf };
}

function viewOf({ x, clip }: PageStep) {
  return { origin: x, left: clip.left - x, right: clip.right - x };
}

const CELLS = Object.fromEntries(
  Array.from({ length: 200 * 40 }, (_, index) => [
    `${Math.floor(index / 40)}:${index % 40}`,
    `t${index}`,
  ]),
);

const SIZES = { sheet: { rows: 200, cols: 40 }, size: { width: 350, height: 190 } };

describe("the pages' quads", () => {
  test("stay as they are wherever the sheet is scrolled to", () => {
    const { pages, show, uploads } = open();
    const cache = cacheOf(CELLS, 4, 3);
    // Rows 1 to 9, which are of two groups of eight.
    show(frameOf({ ...SIZES, cache, scroll: { top: 30, left: 0 } }));
    expect(pages.bandsBuilt).toBe(2);
    for (const [top, left] of [
      [37, 3],
      [120, 400],
      [0, 0],
    ] as const) {
      show(frameOf({ ...SIZES, cache, scroll: { top, left } }));
    }
    expect(pages.bandsBuilt).toBe(2);
    // What is made in one frame is uploaded together.
    expect(uploads()).toHaveLength(1);
    show(frameOf({ ...SIZES, cache, scroll: { top: 190, left: 400 } }));
    expect(pages.bandsBuilt).toBe(3);
    show(frameOf({ ...SIZES, cache, scroll: { top: 30, left: 0 } }));
    expect(pages.bandsBuilt).toBe(3);
  });

  test("are made again for the page that an edit has changed, and only for that", () => {
    const { pages, show } = open();
    const cache = cacheOf(CELLS, 4, 3);
    const frame = { ...SIZES, cache, scroll: { top: 20 * 60, left: 75 * 14 } };
    show(frameOf(frame));
    expect(pages.size).toBe(4);
    const { bandsBuilt } = pages;
    cache.accept(pageOf({ ...CELLS, "63:15": "changed" }, 0, 0));
    // The grid has a new layout after every edit, with the same sizes.
    show(frameOf({ ...frame, layout: axes() }));
    expect(pages.bandsBuilt).toBe(bandsBuilt + 1);
    expect(pages.size).toBe(4);
    show(frameOf({ ...frame, layout: axes([], [[2, 90]]) }));
    // Those with a column of another size, or after one.
    expect(pages.bandsBuilt).toBeGreaterThan(bandsBuilt + 1);
  });

  test("take up no more room than those of the pages that are held", () => {
    const { pages, show } = open();
    const cache = cacheOf(CELLS, 4, 3);
    show(frameOf({ ...SIZES, cache, scroll: { top: 0, left: 0 } }));
    const { bytes } = pages;
    for (let edit = 0; edit < 400; edit++) {
      cache.accept(pageOf({ ...CELLS, "1:1": `edit ${edit}` }, 0, 0));
      show(frameOf({ ...SIZES, cache }));
    }
    expect(pages.bytes).toBe(bytes);
    cache.clear();
    show(frameOf({ ...SIZES, cache }));
    expect(pages.size).toBe(0);
  });

  test("give back buffers that none of them is in any more", () => {
    const { device, show } = open();
    const cache = cacheOf(CELLS, 4, 3);
    show(frameOf({ ...SIZES, cache }));
    const { live } = device;
    // Wide texts of one character each: more quads than a buffer has room for.
    const full = Object.fromEntries(
      Array.from({ length: 64 }, (_, row) => [`${row}:0`, "x".repeat(1500)]),
    );
    cache.accept(pageOf(full, 0, 0));
    const layout = axes([], [[0, 15_000]]);
    show(frameOf({ ...SIZES, cache, layout, size: { width: 350, height: 64 * 20 } }));
    expect(device.live).toBeGreaterThan(live);
    cache.clear();
    // What is let go of may still be read by the frame before.
    show(frameOf({ ...SIZES, cache }));
    show(frameOf({ ...SIZES, cache }));
    expect(device.live).toBe(live);
  });

  test("are not overwritten while a frame that shows them has yet to be submitted", () => {
    const { pages, scenes, glyphs, show } = open();
    const cache = cacheOf({ "0:0": "😀".repeat(600) }, 1, 1);
    const frame = frameOf({ ...SIZES, cache, layout: axes([], [[0, 6000]]) });
    const scene = show(frame);
    const step = scene.steps.find((step) => step.kind === "texts");
    if (step?.kind !== "texts") {
      throw new Error("no texts");
    }
    pages.begin(cache);
    glyphs.begin(Number.POSITIVE_INFINITY);
    const at = (left: number) => {
      const view = { origin: 0, left, right: left + 300 };
      pages.make(scene.look, step.layout, 0, view);
      return pages.shown(scene.look, step.layout, 0, view)?.block;
    };
    const [first, second] = [at(0), at(1500)];
    expect(first?.chunk).toBe(second?.chunk);
    expect(first).not.toBe(second);
    expect((first?.first ?? 0) + (first?.count ?? 0)).toBeLessThanOrEqual(second?.first ?? -1);
    scenes.build(frame);
  });

  test("of a long text are made again when more of it comes into view", () => {
    const { pages, show } = open();
    const cache = cacheOf({ "0:0": "😀".repeat(600) }, 1, 1);
    const layout = axes([], [[0, 9000]]);
    show(frameOf({ ...SIZES, cache, layout }));
    show(frameOf({ ...SIZES, cache, layout, scroll: { top: 0, left: 200 } }));
    expect(pages.bandsBuilt).toBe(1);
    show(frameOf({ ...SIZES, cache, layout, scroll: { top: 0, left: 1500 } }));
    expect(pages.bandsBuilt).toBe(2);
    // Two parts of a frame that show different parts of it do not take turns.
    show(frameOf({ ...SIZES, cache, layout, scroll: { top: 0, left: 1000 } }));
    expect(pages.bandsBuilt).toBe(3);
    show(frameOf({ ...SIZES, cache, layout, scroll: { top: 0, left: 1500 } }));
    show(frameOf({ ...SIZES, cache, layout, scroll: { top: 0, left: 1000 } }));
    expect(pages.bandsBuilt).toBe(3);
  });

  test("are made again when what they are copied from had to make room", () => {
    const { pages, show, glyphs } = open();
    const cache = cacheOf(CELLS, 1, 1);
    const frame = frameOf({ ...SIZES, cache });
    show(frame);
    const { plan } = glyphs.atlas;
    plan.generations = plan.generations.map((generation) => generation + 1);
    show(frame);
    expect(pages.bandsBuilt).toBe(2);
  });

  test("with bars in them are shown until they are made again", () => {
    const { pages, show, glyphs, bandOf } = open();
    const cache = cacheOf(CELLS, 1, 1);
    const frame = frameOf({ ...SIZES, cache });
    const first = show(frame, 0);
    expect(glyphs.deferred).toBeGreaterThan(0);
    expect(bandOf(first)).toMatchObject({ state: "stale", shown: { complete: false } });
    const { bandsBuilt } = pages;
    const second = show(frame);
    expect(pages.bandsBuilt).toBe(bandsBuilt + 1);
    expect(bandOf(second)).toMatchObject({ state: "ready", shown: { complete: true } });
    show(frame);
    expect(pages.bandsBuilt).toBe(bandsBuilt + 1);
  });

  describe("when the sheet is zoomed", () => {
    const zoomed = (zoom: number, changes: Partial<Frame> = {}) => {
      const metrics = { ...METRICS, rowHeight: 20 * zoom, colWidth: 75 * zoom };
      const layout = {
        ...axes(),
        rowAxis: new SparseAxis(metrics.rowHeight),
        colAxis: new SparseAxis(metrics.colWidth),
      };
      return frameOf({ ...SIZES, zoom, metrics, layout, ...changes });
    };

    test("stand for the new ones until those are made", () => {
      const { pages, show, scenes, bandOf } = open();
      const cache = cacheOf(CELLS, 1, 1);
      const before = show(zoomed(1, { cache }));
      const old = bandOf(before);
      expect(old.state).toBe("ready");
      pages.begin(cache);
      const scene = scenes.build(zoomed(1.5, { cache }));
      expect(bandOf(scene)).toEqual({ state: "stale", shown: old.shown });
      // Bars would not be better.
      show(zoomed(1.5, { cache }), 0);
      expect(bandOf(scene)).toEqual({ state: "stale", shown: old.shown });
      const after = bandOf(show(zoomed(1.5, { cache })));
      expect(after.state).toBe("ready");
      expect(after.shown).not.toBe(old.shown);
    });

    test("give back their room when they no longer stand for any", () => {
      const { pages, show } = open();
      const cache = cacheOf(CELLS, 1, 1);
      show(zoomed(1, { cache }));
      const { bytes } = pages;
      for (let round = 0; round < 200; round++) {
        show(zoomed(1 + (round % 7) / 10, { cache }));
      }
      expect(pages.bytes).toBe(bytes);
    });

    test("stand for nothing if that is not all that changed", () => {
      const { pages, show, scenes, bandOf } = open();
      const cache = cacheOf(CELLS, 1, 1);
      show(zoomed(1, { cache }));
      pages.begin(cache);
      const theme = { ...frameOf().theme, background: "#101010" };
      expect(bandOf(scenes.build(zoomed(1.5, { cache, theme }))).state).toBe("missing");
      show(zoomed(1, { cache }));
      pages.begin(cache);
      const layout = axes([], [[0, 90]]);
      expect(bandOf(scenes.build(frameOf({ ...SIZES, cache, zoom: 1, layout }))).state).toBe(
        "missing",
      );
      show(zoomed(1, { cache }));
      cache.accept(pageOf({ ...CELLS, "1:1": "changed" }, 0, 0));
      pages.begin(cache);
      expect(bandOf(scenes.build(zoomed(1.5, { cache }))).state).toBe("missing");
    });

    test("stand for nothing once a size changes at the zoom they are waiting for", () => {
      const { pages, show, scenes, bandOf } = open();
      const cache = cacheOf(CELLS, 1, 1);
      show(zoomed(1, { cache }));
      pages.begin(cache);
      expect(bandOf(scenes.build(zoomed(1.5, { cache }))).state).toBe("stale");
      pages.begin(cache);
      const frame = zoomed(1.5, { cache });
      const colAxis = new SparseAxis(75 * 1.5, [[0, 20]]);
      const resized = { ...frame, layout: { ...axes(), ...frame.layout, colAxis } };
      expect(bandOf(scenes.build(resized)).state).toBe("missing");
    });

    test("stand for nothing if what they are copied from had to make room", () => {
      const { pages, show, scenes, glyphs, bandOf } = open();
      const cache = cacheOf(CELLS, 1, 1);
      show(zoomed(1, { cache }));
      const { plan } = glyphs.atlas;
      plan.generations = plan.generations.map((generation) => generation + 1);
      pages.begin(cache);
      expect(bandOf(scenes.build(zoomed(1.5, { cache }))).state).toBe("missing");
    });

    test("can be let go of", () => {
      const { pages, show, scenes, bandOf } = open();
      const cache = cacheOf(CELLS, 1, 1);
      show(zoomed(1, { cache }));
      expect(pages.dropStale()).toBe(false);
      const scene = scenes.build(zoomed(1.5, { cache }));
      expect(bandOf(scene).state).toBe("stale");
      expect(pages.dropStale()).toBe(true);
      expect(bandOf(scene)).toEqual({ state: "missing", shown: null });
    });
  });

  test("say which rows they are in, and lines through texts are in any", () => {
    const { show, uploads } = open();
    const styles = [PLAIN, { ...PLAIN, strikethrough: true }];
    const cache = cacheOf({ "2:0": { text: "ab", style: 1 }, "5:1": "c" }, 1, 1);
    show(frameOf({ ...SIZES, cache, styles }));
    const bytes = uploads().map(({ args }) => args[2] as Uint8Array);
    const quads = bytes.flatMap((each) => quadsOf(each));
    expect(quads.filter(({ textured }) => textured).map(({ row }) => row)).toEqual([2, 2, 5]);
    expect(quads.filter(({ textured }) => !textured).map(({ row }) => row)).toEqual([0x7f]);
  });

  test("are given back", () => {
    const { device, pages, show } = open();
    const { live } = device;
    show(frameOf({ ...SIZES, cache: cacheOf(CELLS, 1, 1) }));
    expect(device.live).toBeGreaterThan(live);
    pages.destroy();
    expect(device.live).toBe(live);
  });
});
