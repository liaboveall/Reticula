import { PAGE_ROWS } from "@reticula/protocol/viewport";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { SparseAxis, viewMetrics } from "../../layout.ts";
import { axes, cacheOf, colors, frameOf, METRICS, measure, PLAIN, pageOf } from "../fixtures.ts";
import type { Frame } from "../frame.ts";
import type { TextMeasurer } from "../text-measure.ts";
import { Gpu } from "./device.ts";
import { FakeDevice, fakeBrowser, partOf, quadsOf } from "./fakes.ts";
import { faint } from "./glyphs.ts";
import { RetainedPages } from "./pages.ts";
import { WebGpuRenderer } from "./renderer.ts";

beforeEach(() => {
  fakeBrowser();
  // However slow the machine, there is time for everything unless told otherwise.
  vi.spyOn(performance, "now").mockReturnValue(0);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function open() {
  const device = new FakeDevice();
  const gpu = new Gpu(device.asDevice, "fake", "");
  const onIncomplete = vi.fn();
  const release = vi.fn();
  const renderer = new WebGpuRenderer(
    { gpu, release },
    { measure: measure as TextMeasurer, onIncomplete },
  );
  // What finding out how to upload has done.
  device.recorder.clear();
  const frames = () => device.submitted.at(-1) ?? [];
  return { device, gpu, renderer, onIncomplete, release, frames };
}

const CELLS = Object.fromEntries(
  Array.from({ length: 200 * 40 }, (_, index) => [
    `${Math.floor(index / 40)}:${index % 40}`,
    `t${index}`,
  ]),
);

const SIZES = { sheet: { rows: 200, cols: 40 } };

/** The bytes of the quads that a draw call shows, from what was written to its buffer last. */
function quadsDrawn(device: FakeDevice, draw: { buffer: unknown; first: number; count: number }) {
  const quads: ReturnType<typeof quadsOf> = [];
  for (const { args } of device.recorder.named("writeBuffer")) {
    const [buffer, offset, data] = args as [unknown, number, Uint8Array];
    if (buffer === draw.buffer && data instanceof Uint8Array) {
      for (const [index, quad] of quadsOf(data).entries()) {
        quads[offset / 24 + index] = quad;
      }
    }
  }
  return quads.slice(draw.first, draw.first + draw.count);
}

test("everything is drawn in the order of the scene, cut to the part of the sheet it is in", () => {
  const { device, renderer, frames } = open();
  const styles = [
    PLAIN,
    { ...PLAIN, fillColor: { kind: "rgb", rgb: 0x112233 } as const, borderTop: "thin" as const },
  ];
  const cells = { "0:0": { text: "ab", style: 1 }, "3:2": { text: "cd", style: 1 } };
  const frame = frameOf({
    ...SIZES,
    styles,
    cache: cacheOf(cells, 2, 2),
    layout: axes([], [], 1, 1),
    selection: { row: 3, col: 2 },
  });
  const counts = renderer.draw(frame);
  expect(counts.cells).toBe(2);
  const draws = frames();
  const own = draws[0]?.buffer;
  const kinds = draws.map((draw) => (draw.buffer === own ? "own" : "page"));
  // Background; then for each part of the sheet fills, lines, borders, texts and what is over them.
  expect(kinds[0]).toBe("own");
  expect(kinds.at(-1)).toBe("own");
  expect(new Set(draws.map(({ loadOp }) => loadOp))).toEqual(new Set(["clear"]));
  const scissors = draws
    .filter((_, index) => kinds[index] === "page")
    .map(({ scissor }) => scissor);
  // The corner: below the column names and right of the row numbers, one cell large.
  expect(scissors[0]).toEqual([45, 30, 75, 20]);
  expect(scissors.at(-1)).toEqual([120, 50, 350 - 10 - 120, 190 - 10 - 50]);
  // Whole screen, which is what a pass begins with.
  for (const { scissor } of draws.slice(1).filter(({ buffer }) => buffer === own)) {
    expect(scissor).toEqual([0, 0, 350, 190]);
  }
  // Within a part: the fill, and after the lines the border and the text.
  const corner = draws.filter(({ scissor }) => scissor.join() === "45,30,75,20");
  const [fill, border, text] = corner.map((draw) => quadsDrawn(device, draw));
  // All of its page's, of which the rest is cut off.
  expect(fill).toMatchObject([
    { x: 0, y: 0, width: 75, height: 20, color: 0xff332211 },
    { x: 150, y: 60, width: 75, height: 20, color: 0xff332211 },
  ]);
  expect(border?.every(({ textured, height }) => !textured && height === 1)).toBe(true);
  expect(text?.map(({ textured, row }) => [textured, row])).toEqual([
    [true, 0],
    [true, 0],
    [true, 3],
    [true, 3],
  ]);
  const at = (draw: unknown) => draws.indexOf(draw as (typeof draws)[number]);
  expect(at(corner[0])).toBeLessThan(at(corner[1]));
  expect(kinds.slice(at(corner[0]), at(corner[1]))).toContain("own");
});

test("scrolling within what there is uploads nothing but the frame's own quads", () => {
  const { device, renderer } = open();
  const cache = cacheOf(CELLS, 4, 3);
  const at = (top: number, left: number) =>
    renderer.draw(frameOf({ ...SIZES, cache, scroll: { top, left } }));
  const scroll = () => {
    for (const [top, left] of [
      [0, 0],
      [17, 33],
      [60, 90],
      [100, 20],
    ] as const) {
      at(top, left);
    }
  };
  // Until there is nothing around it to get ready either.
  for (let count = 0; count < 5; count++) {
    scroll();
  }
  const before = renderer.counters;
  device.recorder.clear();
  scroll();
  const after = renderer.counters;
  expect(after.frames).toBe(before.frames + 4);
  expect(after.pageUploads).toBe(before.pageUploads);
  expect(after.bandsBuilt).toBe(before.bandsBuilt);
  expect(after.atlasUploads).toBe(before.atlasUploads);
  // The size of the screen, the frame's own quads, and where the parts are.
  expect(device.recorder.named("writeBuffer")).toHaveLength(4 * 3);
  expect(device.recorder.named("copyExternalImageToTexture")).toHaveLength(0);
});

test("the texts of a page are copied to the atlas together", () => {
  const { device, renderer } = open();
  renderer.draw(frameOf({ ...SIZES, cache: cacheOf(CELLS, 1, 1) }));
  expect(device.recorder.named("copyExternalImageToTexture")).toHaveLength(1);
  expect(renderer.counters.atlasUploads).toBe(1);
  expect(renderer.counters.atlasMisses).toBeGreaterThan(20);
});

describe("what comes into view next is made ready", () => {
  const cache = cacheOf(CELLS, 4, 3);
  const at = (top: number, left = 0): Frame =>
    frameOf({ ...SIZES, cache, scroll: { top, left }, size: { width: 350, height: 190 } });

  test("one page a frame, when nothing that is in view was not there", () => {
    const { renderer } = open();
    renderer.draw(at(30));
    expect(renderer.counters.bandsBuilt).toBe(2);
    // Nothing is ahead as long as nothing moves.
    renderer.draw(at(30));
    expect(renderer.counters.bandsBuilt).toBe(2);
    renderer.draw(at(40));
    // The page has six more groups of rows.
    expect(renderer.counters.bandsBuilt).toBe(PAGE_ROWS / 8);
    renderer.draw(at(60));
    const ready = renderer.counters.bandsBuilt;
    expect(ready).toBeGreaterThan(PAGE_ROWS / 8);
    expect(ready).toBeLessThanOrEqual(2 * (PAGE_ROWS / 8));
    const { pageUploads } = renderer.counters;
    renderer.draw(at(20 * 30));
    expect(renderer.counters.pageUploads - pageUploads).toBeLessThanOrEqual(PAGE_ROWS / 8);
  });

  test("sideways too", () => {
    const { renderer } = open();
    renderer.draw(at(0, 0));
    const { bandsBuilt } = renderer.counters;
    renderer.draw(at(0, 10));
    const ready = renderer.counters.bandsBuilt;
    expect(ready).toBeGreaterThan(bandsBuilt);
    renderer.draw(at(0, 75 * 15 + 1));
    // Nothing new is in view, and the page after that gets ready.
    expect(renderer.counters.bandsBuilt).toBe(ready + 1);
  });
});

describe("texts there is no time to draw", () => {
  test("are drawn in the frames after it, which it asks for", () => {
    const { renderer, onIncomplete } = open();
    let now = 0;
    vi.spyOn(performance, "now").mockImplementation(() => (now += 1));
    const frame = frameOf({ ...SIZES, cache: cacheOf(CELLS, 1, 1) });
    renderer.draw(frame);
    expect(onIncomplete).toHaveBeenCalledTimes(1);
    expect(renderer.counters.deferredFrames).toBe(1);
    let frames = 1;
    for (; onIncomplete.mock.calls.length === frames; frames++) {
      expect(frames).toBeLessThan(200);
      renderer.draw(frame);
    }
    const { atlasMisses, deferredFrames } = renderer.counters;
    renderer.draw(frame);
    expect(renderer.counters).toMatchObject({ atlasMisses, deferredFrames });
    expect(onIncomplete).toHaveBeenCalledTimes(deferredFrames);
  });

  test("can be drawn before anything is shown", () => {
    const { device, renderer } = open();
    let now = 0;
    vi.spyOn(performance, "now").mockImplementation(() => (now += 1));
    const frame = frameOf({ ...SIZES, cache: cacheOf(CELLS, 1, 1) });
    let rounds = 0;
    while (!renderer.warm(frame)) {
      expect(rounds++).toBeLessThan(200);
    }
    expect(rounds).toBeGreaterThan(2);
    expect(device.submitted).toHaveLength(0);
    const { bandsBuilt } = renderer.counters;
    renderer.draw(frame);
    expect(renderer.counters.bandsBuilt).toBe(bandsBuilt);
    expect(device.submitted).toHaveLength(1);
  });
});

describe("a frame that has no time for all of its texts", () => {
  const cache = cacheOf(CELLS, 4, 3);
  const size = { width: 700, height: 600 };
  const zoomed = (zoom: number, changes: Partial<Frame> = {}): Frame =>
    frameOf({
      ...SIZES,
      cache,
      size,
      zoom,
      metrics: viewMetrics(METRICS, zoom, 1),
      layout: {
        ...axes(),
        rowAxis: new SparseAxis(METRICS.rowHeight, [], 1, zoom),
        colAxis: new SparseAxis(METRICS.colWidth, [], 1, zoom),
      },
      ...changes,
    });

  /** Has making each group of rows take a while, and nothing else. */
  function timed(each: number) {
    let now = 0;
    vi.spyOn(performance, "now").mockImplementation(() => now);
    const { make } = RetainedPages.prototype;
    const states: string[] = [];
    vi.spyOn(RetainedPages.prototype, "make").mockImplementation(function (
      this: RetainedPages,
      ...args
    ) {
      states.push(this.stateOf(...args));
      now += clock.each;
      make.apply(this, args);
    });
    const clock = { each, states };
    return clock;
  }

  const settle = ({ renderer, onIncomplete }: ReturnType<typeof open>, frame: Frame): number => {
    for (let count = 1; count < 100; count++) {
      onIncomplete.mockClear();
      renderer.draw(frame);
      if (onIncomplete.mock.calls.length === 0) {
        return count;
      }
    }
    throw new Error("some of the texts never come");
  };

  /** What is drawn of pages, and where. */
  const pagesDrawn = ({ device, frames }: ReturnType<typeof open>) => {
    const [own] = frames();
    return frames()
      .filter(({ buffer }) => buffer !== own?.buffer)
      .map((draw) => ({ ...partOf(device, draw.part), quads: quadsDrawn(device, draw) }));
  };
  const ownDrawn = ({ device, frames }: ReturnType<typeof open>) => {
    const [own] = frames();
    return frames()
      .filter(({ buffer }) => buffer === own?.buffer)
      .flatMap((draw) => quadsDrawn(device, draw));
  };
  const BAR = faint(colors.parse(frameOf().theme.text));

  test("makes those it has time for, and shows bars for the others", () => {
    const opened = open();
    const clock = timed(5);
    opened.renderer.draw(zoomed(1));
    expect(opened.renderer.counters).toMatchObject({ bandsBuilt: 3, deferredFrames: 1 });
    expect(opened.onIncomplete).toHaveBeenCalledTimes(1);
    const bars = ownDrawn(opened).filter(({ color, textured }) => !textured && color === BAR);
    // Every cell in view has a text, but for those of three groups of eight rows.
    const drawn = opened.renderer.draw(zoomed(1)).cells;
    expect(bars.length).toBe(drawn - 3 * 8 * 9);
    for (const { x, y, width, height } of bars) {
      expect(x).toBeGreaterThanOrEqual(45 + 6);
      expect(x + width).toBeLessThanOrEqual(700 - 10);
      // Below the groups of rows that are there.
      expect(y).toBeGreaterThanOrEqual(30 + 3 * 8 * 20);
      expect(y + height).toBeLessThanOrEqual(600 - 10);
    }
    clock.each = 0;
    settle(opened, zoomed(1));
    expect(ownDrawn(opened).some(({ color }) => color === BAR)).toBe(false);
  });

  test("stretches those there were before the sheet was zoomed, until it has made them anew", () => {
    const opened = open();
    const clock = timed(0);
    settle(opened, zoomed(1));
    const before = pagesDrawn(opened);
    const { deferredFrames, pageUploads } = opened.renderer.counters;
    // While it is being zoomed, the next frame may have no use for what one makes.
    clock.each = 100;
    const zoom = 1.5;
    opened.renderer.draw(zoomed(zoom, { size: { width: 450, height: 400 } }));
    expect(opened.onIncomplete).toHaveBeenCalledTimes(1);
    expect(opened.renderer.counters.deferredFrames).toBe(deferredFrames + 1);
    const stretched = pagesDrawn(opened).filter(({ down }) => down !== 1);
    expect(stretched.length).toBeGreaterThan(0);
    for (const { x, across, down, quads } of stretched) {
      expect([x, across, down]).toEqual([68, 113 / 75, 1.5].map(Math.fround));
      // The very quads that were drawn before.
      const [first] = quads;
      expect(before.some((each) => each.quads.some((quad) => sameQuad(quad, first)))).toBe(true);
    }
    // The only ones that were begun would have been bars.
    expect(opened.renderer.counters.pageUploads).toBe(pageUploads);

    clock.each = 0;
    settle(opened, zoomed(zoom));
    const fresh = open();
    settle(fresh, zoomed(zoom));
    const shapes = (each: ReturnType<typeof pagesDrawn>) =>
      each.flatMap(({ quads, ...part }) =>
        quads.map(({ x, y, width, height, row, textured, color }) =>
          [part, x, y, width, height, row, textured, color].map((one) => JSON.stringify(one)),
        ),
      );
    expect(shapes(pagesDrawn(opened)).sort()).toEqual(shapes(pagesDrawn(fresh)).sort());
    expect(pagesDrawn(opened).every(({ across, down }) => across === 1 && down === 1)).toBe(true);
    expect(opened.renderer.counters.deferredFrames).toBe(deferredFrames + 1);
  });

  test("makes those first that nothing stands for", () => {
    const opened = open();
    const clock = timed(0);
    settle(opened, zoomed(1));
    clock.states.length = 0;
    opened.renderer.draw(zoomed(0.5));
    const { states } = clock;
    expect(states.filter((state) => state === "stale").length).toBeGreaterThan(2);
    expect(states.indexOf("stale")).toBeGreaterThan(2);
    expect(states.lastIndexOf("missing")).toBeLessThan(states.indexOf("stale"));
  });

  test("has less time while the sheet is zoomed, and goes on when that is over", () => {
    const opened = open();
    const clock = timed(0);
    settle(opened, zoomed(1));
    clock.each = 4;
    const made = (frame: Frame) => {
      clock.states.length = 0;
      opened.renderer.draw(frame);
      return clock.states.length;
    };
    const zooms = [0.9, 0.8, 0.5, 0.5];
    expect(zooms.map((zoom) => made(zoomed(zoom)))).toEqual([2, 2, 2, 3]);
    // Another theme is not another zoom.
    const theme = { ...frameOf().theme, text: "#123456" };
    expect(made(zoomed(0.5, { theme }))).toBe(3);
  });

  test("shows the headers' labels all the same", () => {
    const opened = open();
    const clock = timed(0);
    settle(opened, zoomed(1));
    const labels = ownDrawn(opened).filter(({ textured }) => textured).length;
    clock.each = 1000;
    opened.renderer.draw(zoomed(1.25));
    expect(opened.renderer.counters.deferredFrames).toBe(1);
    expect(ownDrawn(opened).filter(({ textured }) => textured).length).toBeGreaterThan(labels / 2);
    expect(ownDrawn(opened).some(({ color, x }) => color === BAR && x < 45)).toBe(false);
  });

  test("makes room for them at the cost of what stands for others", () => {
    const opened = open();
    const clock = timed(0);
    settle(opened, zoomed(1));
    const { plan } = opened.gpu.glyphs.atlas;
    // All of the atlas but a part of a strip holds what is in view.
    plan.use = () => {};
    let full = true;
    const { reserve } = opened.gpu.glyphs.atlas;
    vi.spyOn(opened.gpu.glyphs.atlas, "reserve").mockImplementation(function (
      this: typeof opened.gpu.glyphs.atlas,
      width,
      height,
    ) {
      if (full) {
        full = false;
        this.full = true;
        return null;
      }
      return reserve.call(this, width, height);
    });
    const dropped = vi.spyOn(RetainedPages.prototype, "dropStale");
    clock.states.length = 0;
    opened.renderer.draw(zoomed(1.5));
    expect(dropped).toHaveReturnedWith(true);
    expect(clock.states).toContain("missing");
    expect(opened.renderer.counters.extraSubmits).toBe(0);
    expect(opened.onIncomplete).not.toHaveBeenCalled();
  });
});

const sameQuad = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

test("more than the atlas holds is drawn in several steps, all of it", () => {
  const { device, renderer, onIncomplete, frames } = open();
  // Every row shows a little of a picture of its own, which is far larger.
  const rows = 192;
  const cells = Object.fromEntries(
    Array.from({ length: rows }, (_, row) => [`${row}:0`, String.fromCodePoint(0x1f300 + row)]),
  );
  const theme = { ...frameOf().theme, fontSize: 480 };
  const frame = frameOf({
    ...SIZES,
    theme,
    cache: cacheOf(cells, 3, 1),
    size: { width: 1200, height: rows * 20 },
    scroll: { top: 0, left: 0 },
  });
  renderer.draw(frame);
  const first = renderer.counters;
  if (first.deferredFrames > 0) {
    for (let count = 0; count < 100 && renderer.counters.extraSubmits === 0; count++) {
      renderer.draw(frame);
    }
  }
  const { extraSubmits } = renderer.counters;
  expect(extraSubmits).toBeGreaterThan(0);
  expect(renderer.counters.atlasLayers).toBe(4);
  onIncomplete.mockClear();
  device.submitted.length = 0;
  renderer.draw(frame);
  // Nothing is missing, so that no other frame follows; the parts add up.
  expect(onIncomplete).not.toHaveBeenCalled();
  expect(device.submitted.length).toBeGreaterThan(1);
  expect(device.submitted.map((draws) => draws[0]?.loadOp)).toEqual(
    device.submitted.map((_, index) => (index === 0 ? "clear" : "load")),
  );
  expect(frames().length).toBeGreaterThan(0);
});

/**
 * Follows what texts drawn whole are copied from: from the canvas they are drawn
 * on, through the atlas, to what the GPU is told to draw when a frame is submitted.
 */
function followTexts({ device, gpu }: Pick<ReturnType<typeof open>, "device" | "gpu">) {
  interface Picture {
    readonly text: string;
    readonly layer: number;
    readonly x: number;
    readonly y: number;
    readonly width: number;
    readonly height: number;
  }
  const isWithin = (a: Omit<Picture, "text">, b: Omit<Picture, "text">) =>
    a.layer === b.layer &&
    a.x >= b.x &&
    a.y >= b.y &&
    a.x + a.width <= b.x + b.width &&
    a.y + a.height <= b.y + b.height;
  const isOn = (a: Omit<Picture, "text">, b: Omit<Picture, "text">) =>
    a.layer === b.layer &&
    a.x < b.x + b.width &&
    b.x < a.x + a.width &&
    a.y < b.y + b.height &&
    b.y < a.y + a.height;
  let canvas: Picture[] = [];
  let atlas: Picture[] = [];
  let box = [0, 0, 0, 0];
  const context = gpu.glyphs.atlas.raster.context as unknown as Record<string, unknown>;
  Object.assign(context, {
    clearRect: () => {
      canvas = [];
    },
    rect: (...args: number[]) => {
      box = args;
    },
    fillText: (text: string) => {
      const [x = 0, y = 0, width = 0, height = 0] = box;
      canvas.push({ text, layer: 0, x, y, width, height });
      box = [0, 0, 0, 0];
    },
  });
  vi.spyOn(device.queue, "copyExternalImageToTexture").mockImplementation((...args: unknown[]) => {
    const [{ origin }, { origin: to }, [width, height]] = args as [
      { origin: [number, number] },
      { origin: { x: number; y: number; z: number } },
      [number, number],
    ];
    const copied = { layer: to.z, x: to.x, y: to.y, width, height };
    atlas = [
      ...atlas.filter((picture) => !isOn(picture, copied)),
      ...canvas
        .map((picture) => ({
          ...picture,
          layer: to.z,
          x: picture.x - origin[0] + to.x,
          y: picture.y - origin[1] + to.y,
        }))
        .filter((picture) => picture.width > 0 && isWithin(picture, copied)),
    ];
  });
  /** The texts that were copied to each row of the page they are of, by frame. */
  const shown: { readonly row: number; readonly text: string | undefined }[] = [];
  const submit = device.queue.submit;
  vi.spyOn(device.queue, "submit").mockImplementation((buffers) => {
    submit(buffers);
    for (const draw of buffers.flatMap(({ draws }) => draws)) {
      expect(draw.isStale).toBe(false);
      for (const { row, textured, layer, u, v, width, height } of quadsDrawn(device, draw)) {
        if (textured && row < PAGE_ROWS) {
          const quad = { layer, x: u, y: v, width, height };
          shown.push({ row, text: atlas.find((picture) => isWithin(quad, picture))?.text });
        }
      }
    }
  });
  return shown;
}

test("what is drawn is copied from the pictures that were meant", () => {
  const opened = open();
  const { renderer, onIncomplete } = opened;
  const shown = followTexts(opened);
  const rows = 192;
  const textOf = (row: number) => String.fromCodePoint(0x1f300 + row);
  const cells = Object.fromEntries(
    Array.from({ length: rows }, (_, row) => [`${row}:0`, textOf(row)]),
  );
  const theme = { ...frameOf().theme, fontSize: 480 };
  const cache = cacheOf(cells, 3, 1);
  const draw = (height: number, top: number) => {
    const size = { width: 1200, height };
    renderer.draw(frameOf({ ...SIZES, theme, cache, size, scroll: { top, left: 0 } }));
  };
  const expectRight = () => {
    for (const { row, text } of shown) {
      expect([0, 1, 2].map((page) => textOf(page * PAGE_ROWS + row))).toContain(text);
    }
  };
  // Back and forth, with some of the atlas in use and some making room.
  for (const top of [0, 300, 900, 1500, 2100, 2700, 1800, 1200, 600, 0, 2400]) {
    for (let count = 0; count < 100; count++) {
      onIncomplete.mockClear();
      draw(400, top);
      if (onIncomplete.mock.calls.length === 0) {
        break;
      }
    }
    expect(onIncomplete).not.toHaveBeenCalled();
  }
  expect(renderer.counters.atlasEvictions).toBeGreaterThan(0);
  expect(renderer.counters.extraSubmits).toBe(0);
  expectRight();
  // All of it at once, which takes several steps.
  for (let count = 0; count < 100 && renderer.counters.extraSubmits === 0; count++) {
    draw(rows * 20, 0);
  }
  shown.length = 0;
  onIncomplete.mockClear();
  const { extraSubmits } = renderer.counters;
  draw(rows * 20, 0);
  expect(renderer.counters.extraSubmits).toBeGreaterThan(extraSubmits);
  expect(onIncomplete).not.toHaveBeenCalled();
  expectRight();
  const texts = new Set(shown.map(({ text }) => text));
  // But for the rows that the headers and the edge of the view leave no room for.
  expect(texts.size).toBeGreaterThan(rows - 8);
});

test("a page that was edited is uploaded again, and no other", () => {
  const { renderer } = open();
  const cache = cacheOf(CELLS, 4, 3);
  const frame = { ...SIZES, cache, scroll: { top: 20 * 60, left: 75 * 14 } };
  renderer.draw(frameOf(frame));
  renderer.draw(frameOf(frame));
  const before = renderer.counters;
  cache.accept(pageOf({ ...CELLS, "63:15": "changed" }, 0, 0));
  renderer.draw(frameOf({ ...frame, layout: axes() }));
  const after = renderer.counters;
  expect(after.bandsBuilt).toBe(before.bandsBuilt + 1);
  expect(after.pageUploads).toBe(before.pageUploads + 1);
});

test("everything it has is given back with it", () => {
  const { device, gpu, renderer, release } = open();
  const { live } = device;
  renderer.draw(frameOf({ ...SIZES, cache: cacheOf(CELLS, 1, 1) }));
  expect(device.live).toBeGreaterThan(live);
  renderer.destroy();
  expect(release).toHaveBeenCalledTimes(1);
  expect((renderer.canvas as unknown as { removed: boolean }).removed).toBe(true);
  // The atlas is the page's.
  gpu.close();
  expect(device.live).toBe(0);
  expect(device.destroyed).toBe(true);
});

test("a device that is gone is left alone", () => {
  const { device, gpu, renderer } = open();
  const frame = frameOf({ ...SIZES, cache: cacheOf(CELLS, 1, 1) });
  renderer.draw(frame);
  gpu.lose();
  device.recorder.clear();
  expect(renderer.draw(frame).cells).toBeGreaterThan(0);
  expect(device.recorder.calls).toEqual([]);
});
