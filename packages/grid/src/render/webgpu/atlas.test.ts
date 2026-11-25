import { describe, expect, test, vi } from "vitest";
import { Atlas, AtlasPlan, Shelves } from "./atlas.ts";
import { FakeDevice } from "./fakes.ts";
import type { Raster } from "./raster.ts";

describe("shelves", () => {
  test("hold rectangles of any size without overlap", () => {
    const shelves = new Shelves(256, 128);
    const placed: { x: number; y: number; width: number; height: number }[] = [];
    let state = 7;
    const next = (bound: number) => {
      state = (state * 1103515245 + 12345) % 2 ** 31;
      return 1 + (state % bound);
    };
    for (let count = 0; count < 400; count++) {
      const width = next(30);
      const height = next(24);
      const place = shelves.place(width, height);
      if (place) {
        placed.push({ ...place, width, height });
      }
    }
    expect(placed.length).toBeGreaterThan(80);
    for (const [index, a] of placed.entries()) {
      expect(a.x + a.width).toBeLessThanOrEqual(256);
      expect(a.y + a.height).toBeLessThanOrEqual(128);
      for (const b of placed.slice(index + 1)) {
        const apart =
          a.x + a.width <= b.x ||
          b.x + b.width <= a.x ||
          a.y + a.height <= b.y ||
          b.y + b.height <= a.y;
        expect(apart).toBe(true);
      }
    }
  });

  test("are filled with what is about as high, and with anything when there is no room for more", () => {
    const shelves = new Shelves(100, 20);
    expect(shelves.place(10, 16)).toEqual({ x: 0, y: 0 });
    expect(shelves.place(10, 13)).toEqual({ x: 10, y: 0 });
    // Four rows are left, and these are put there rather than on the high shelf.
    expect(shelves.place(10, 4)).toEqual({ x: 0, y: 16 });
    expect(shelves.place(90, 3)).toEqual({ x: 10, y: 16 });
    expect(shelves.place(10, 3)).toEqual({ x: 20, y: 0 });
    expect(shelves.place(10, 17)).toBeNull();
    expect(shelves.place(101, 1)).toBeNull();
  });
});

describe("the plan of an atlas", () => {
  const fill = (plan: AtlasPlan, uses = true) => {
    for (;;) {
      plan.place(64, 16);
      if (uses) {
        plan.use(1 << plan.layer);
      }
      if (!plan.place(64, 16)) {
        return;
      }
    }
  };

  test("opens the strips of a layer, then new layers as far as there may be more", () => {
    const plan = new AtlasPlan(64, 16, 2);
    const seen: [number, number][] = [];
    for (let strip = 0; strip < 8; strip++) {
      seen.push([plan.layer, plan.top]);
      fill(plan);
      if (strip < 7) {
        expect(plan.advance()).toBe(-1);
      }
    }
    expect(seen).toEqual([
      [0, 0],
      [0, 16],
      [0, 32],
      [0, 48],
      [1, 0],
      [1, 16],
      [1, 32],
      [1, 48],
    ]);
    expect(plan.layers).toBe(2);
  });

  test("empties the layer that has not been used for longest, and none in use", () => {
    const plan = new AtlasPlan(16, 16, 3);
    plan.use(1);
    plan.now++;
    expect(plan.advance()).toBe(-1);
    plan.use(1 << 1);
    plan.now++;
    expect(plan.advance()).toBe(-1);
    plan.use(1 << 2);
    plan.now++;
    plan.use(1);
    const before = plan.generations;
    expect(plan.advance()).toBe(1);
    expect(plan.layer).toBe(1);
    expect(plan.generations).toEqual([0, 1, 0]);
    // What was copied from the other layers is still there.
    expect(plan.holds(0b101, before)).toBe(true);
    expect(plan.holds(0b010, before)).toBe(false);
    expect(plan.holds(0b111, before)).toBe(false);
    expect(plan.holds(0b111, plan.generations)).toBe(true);
    plan.use(0b111);
    expect(plan.advance()).toBeNull();
    plan.now++;
    expect(plan.advance()).not.toBeNull();
  });
});

function fakeAtlas(kind: Raster["kind"] = "copy") {
  const device = new FakeDevice();
  const upload = vi.fn<Raster["upload"]>();
  const clear = vi.fn();
  const evicted: number[] = [];
  const raster = { kind, context: {} as CanvasRenderingContext2D, clear, upload };
  const atlas = new Atlas(
    device.asDevice,
    raster,
    (allocate) => allocate(),
    (layer) => evicted.push(layer),
  );
  return { atlas, device, upload, clear, evicted };
}

describe("an atlas", () => {
  test("uploads what a frame has added in one piece", () => {
    const { atlas, upload } = fakeAtlas();
    atlas.reserve(10, 12);
    atlas.reserve(20, 40);
    atlas.reserve(30, 12);
    atlas.flush();
    expect(upload).toHaveBeenCalledTimes(1);
    expect(upload.mock.calls[0]?.slice(1)).toEqual([0, 0, 40, 52, { x: 0, y: 0, z: 0 }]);
    atlas.flush();
    expect(upload).toHaveBeenCalledTimes(1);
    atlas.reserve(8, 40);
    atlas.flush();
    expect(upload.mock.calls[1]?.slice(1)).toEqual([20, 12, 8, 40, { x: 20, y: 12, z: 0 }]);
    expect(atlas.uploads).toBe(2);
    expect(atlas.uploadedBytes).toBe((40 * 52 + 8 * 40) * 4);
  });

  test("says what uploading will take, going by what it took for pictures before", () => {
    const { atlas, upload } = fakeAtlas();
    let now = 0;
    vi.spyOn(performance, "now").mockImplementation(() => now);
    upload.mockImplementation(() => {
      now += 4;
    });
    const draw = (count: number) => {
      for (let drawn = 0; drawn < count; drawn++) {
        atlas.reserve(4, 4);
      }
    };
    draw(40);
    expect(atlas.due).toBe(0);
    atlas.flush();
    draw(10);
    expect(atlas.due).toBeCloseTo((10 * (4 / 40)) / 2, 9);
    // Of so few, that says nothing.
    atlas.flush();
    expect(atlas.due).toBe(0);
    draw(40);
    expect(atlas.due).toBeCloseTo((40 * (4 / 40)) / 2, 9);
    vi.restoreAllMocks();
  });

  test("does not go on expecting uploads to be slow because one was", () => {
    const { atlas, upload } = fakeAtlas();
    let now = 0;
    let takes = 100;
    vi.spyOn(performance, "now").mockImplementation(() => now);
    upload.mockImplementation(() => {
      now += takes;
    });
    /** As many as a frame of five milliseconds has time for. */
    const frame = () => {
      let drawn = 0;
      for (; drawn < 60 && atlas.due < 5; drawn++) {
        atlas.reserve(4, 4);
      }
      atlas.flush();
      return drawn;
    };
    expect(frame()).toBe(60);
    takes = 1;
    expect(frame()).toBeLessThan(10);
    for (let count = 0; count < 10; count++) {
      frame();
    }
    expect(frame()).toBe(60);
    vi.restoreAllMocks();
  });

  test("writes only what has been drawn when that costs by the pixel", () => {
    const { atlas, upload } = fakeAtlas("write");
    atlas.reserve(10, 12);
    atlas.reserve(20, 40);
    atlas.reserve(30, 12);
    atlas.flush();
    expect(upload.mock.calls.map((call) => call.slice(1, 5))).toEqual([
      [0, 0, 40, 12],
      [0, 12, 20, 40],
    ]);
  });

  test("fills no more than a strip a frame, unless it has to", () => {
    const { atlas, upload, clear } = fakeAtlas();
    for (let count = 0; count < 4; count++) {
      expect(atlas.reserve(512, 512)).toEqual({ x: count * 512, y: 0 });
    }
    expect(atlas.reserve(512, 512)).toBeNull();
    expect(atlas.full).toBe(false);
    expect(upload).not.toHaveBeenCalled();
    atlas.flush();
    expect(atlas.reserve(512, 512)).toEqual({ x: 0, y: 0 });
    expect(atlas.plan.top).toBe(512);
    expect(clear).toHaveBeenCalledTimes(1);
    atlas.unlimited = true;
    for (let count = 1; count < 5; count++) {
      expect(atlas.reserve(512, 512)).not.toBeNull();
    }
    expect(atlas.plan.top).toBe(1024);
    // What was drawn into the strip before has been uploaded first.
    expect(upload).toHaveBeenCalledTimes(2);
    expect(clear).toHaveBeenCalledTimes(2);
  });

  test("grows by a layer, keeping what there is, and then empties layers", () => {
    const { atlas, device, evicted } = fakeAtlas();
    const { view } = atlas;
    atlas.unlimited = true;
    const fillLayer = () => {
      for (let count = 0; count < 16; count++) {
        expect(atlas.reserve(512, 512)).not.toBeNull();
      }
    };
    fillLayer();
    expect(atlas.size).toBe(2048 * 2048 * 4);
    fillLayer();
    const sizes = () =>
      device.recorder
        .named("createTexture")
        .map(({ args }) => (args[0] as { size: number[] }).size);
    expect(sizes()).toEqual([
      [2048, 2048, 1],
      [2048, 2048, 2],
    ]);
    expect(device.recorder.named("copyTextureToTexture")[0]?.args[2]).toEqual([2048, 2048, 1]);
    expect(atlas.version).toBe(1);
    expect(atlas.view).not.toBe(view);
    expect(device.live).toBe(1);
    fillLayer();
    fillLayer();
    expect(atlas.size).toBe(64 << 20);
    atlas.plan.use(0b1111);
    expect(atlas.reserve(512, 512)).toBeNull();
    expect(atlas.full).toBe(true);
    atlas.plan.now++;
    atlas.plan.use(0b1101);
    expect(atlas.reserve(512, 512)).not.toBeNull();
    expect(evicted).toEqual([1]);
    expect(atlas.evictions).toBe(1);
    expect(sizes()).toHaveLength(4);
  });
});
