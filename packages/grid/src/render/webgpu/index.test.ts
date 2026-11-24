import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { cacheOf, frameOf, measure } from "../fixtures.ts";
import type { Frame } from "../frame.ts";
import type { TextMeasurer } from "../text-measure.ts";
import type { GridRenderer } from "../types.ts";
import type { RendererHost } from "../upgrade.ts";
import { FakeDevice, fakeBrowser } from "./fakes.ts";

beforeEach(() => {
  vi.useFakeTimers();
  // What a page has been through stays with it.
  vi.resetModules();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

/** A grid as far as its renderers are concerned. */
function fakeGrid(frame: Frame | null = frameOf({ cache: cacheOf({ "0:0": "ab" }, 1, 1) })) {
  const reports: unknown[] = [];
  const grid = {
    renderer: null as GridRenderer | null,
    dragging: false,
    destroyed: false,
    swaps: 0,
    frame,
    reports,
  };
  const host: RendererHost = {
    measure: measure as TextMeasurer,
    frame: () => grid.frame,
    invalidate: vi.fn(),
    swap: (renderer) => {
      if (grid.destroyed || (renderer && grid.dragging)) {
        return false;
      }
      grid.renderer?.destroy();
      grid.renderer = renderer;
      grid.swaps++;
      return true;
    },
    report: (reason, adapter) => reports.push(adapter ? [reason, adapter] : reason),
  };
  return { grid, host };
}

async function attach(host: RendererHost, software = false) {
  const { attach } = await import("./index.ts");
  return attach(host, software);
}

/** Lets everything go on that waits for something else than time. */
const settle = () => vi.advanceTimersByTimeAsync(10);

test("a grid is drawn with WebGPU once the first frame needs nothing any more", async () => {
  const { devices } = fakeBrowser();
  const { grid, host } = fakeGrid();
  await attach(host);
  await settle();
  expect(grid.renderer?.kind).toBe("webgpu");
  expect(grid.reports).toEqual([[null, "acme fast"]]);
  expect(devices).toHaveLength(1);
  // Nothing has been shown yet, and nothing is left to draw into the atlas.
  expect(devices[0]?.submitted).toEqual([]);
  expect(grid.renderer?.counters).toMatchObject({ frames: 0, bandsBuilt: 1, deferredFrames: 0 });
  expect(grid.renderer?.counters?.atlasMisses).toBeGreaterThan(0);
});

test("not before there has been a frame, and not while something is dragged", async () => {
  fakeBrowser();
  const { grid, host } = fakeGrid(null);
  await attach(host);
  await settle();
  expect(grid.renderer).toBeNull();
  grid.frame = frameOf({ cache: cacheOf({}, 1, 1) });
  grid.dragging = true;
  await vi.advanceTimersByTimeAsync(500);
  expect(grid.renderer).toBeNull();
  grid.dragging = false;
  await vi.advanceTimersByTimeAsync(200);
  expect(grid.renderer?.kind).toBe("webgpu");
});

describe("without", () => {
  test("an adapter there is a reason", async () => {
    fakeBrowser(null);
    const { grid, host } = fakeGrid();
    await attach(host);
    await settle();
    expect(grid.reports).toEqual(["no-adapter"]);
  });

  test("a hardware adapter too, unless a software one is wanted", async () => {
    const { devices } = fakeBrowser({ isFallbackAdapter: true });
    const { grid, host } = fakeGrid();
    await attach(host);
    await settle();
    expect(grid.reports).toEqual(["software-adapter"]);
    expect(devices).toHaveLength(0);
    await attach(host, true);
    await settle();
    expect(grid.renderer?.kind).toBe("webgpu");
  });

  test("a context for its canvas the device is given back", async () => {
    const { devices } = fakeBrowser();
    const create = document.createElement;
    vi.stubGlobal("document", {
      documentElement: { lang: "" },
      createElement: (name: string) => {
        const canvas = create.call(document, name) as HTMLCanvasElement;
        const { getContext } = canvas;
        return Object.assign(canvas, {
          getContext: (kind: string) =>
            kind === "webgpu" ? null : (getContext as (kind: string) => unknown).call(canvas, kind),
        });
      },
    });
    const { grid, host } = fakeGrid();
    await attach(host);
    await settle();
    expect(grid.reports).toEqual(["failed"]);
    expect(devices[0]?.destroyed).toBe(true);
  });
});

describe("a device", () => {
  test("is shared by the grids of a page, and closed with the last of them", async () => {
    const { devices } = fakeBrowser();
    const first = fakeGrid();
    const second = fakeGrid();
    await attach(first.host);
    await attach(second.host);
    await settle();
    expect([first, second].map(({ grid }) => grid.renderer?.kind)).toEqual(["webgpu", "webgpu"]);
    expect(devices).toHaveLength(1);
    first.grid.renderer?.destroy();
    expect(devices[0]?.destroyed).toBe(false);
    second.grid.renderer?.destroy();
    expect(devices[0]?.destroyed).toBe(true);
    expect(devices[0]?.live).toBe(0);
    // That was on purpose: nothing is reported, and the next grid gets another one.
    await settle();
    expect(first.grid.reports).toEqual([[null, "acme fast"]]);
    const third = fakeGrid();
    await attach(third.host);
    await settle();
    expect(devices).toHaveLength(2);
    expect(third.grid.renderer?.kind).toBe("webgpu");
  });

  test("is not kept by a grid that was closed before it was ready", async () => {
    const { devices } = fakeBrowser();
    const { grid, host } = fakeGrid();
    const stop = await attach(host);
    grid.destroyed = true;
    stop();
    await settle();
    expect(grid.swaps).toBe(0);
    expect(devices.every(({ destroyed }) => destroyed)).toBe(true);
    // Nor while it is getting ready.
    const other = fakeGrid(null);
    const stopOther = await attach(other.host);
    await settle();
    expect(devices.at(-1)?.destroyed).toBe(false);
    stopOther();
    await vi.advanceTimersByTimeAsync(500);
    expect(devices.at(-1)?.destroyed).toBe(true);
    expect(other.grid.swaps).toBe(0);
  });

  const losses: [string, (device: FakeDevice) => void][] = [
    ["is destroyed", (device) => device.destroy()],
    ["reports an error", (device) => device.dispatchEvent(new Event("uncapturederror"))],
  ];
  test.each(losses)(
    "that %s is given up for a 2D canvas, and asked for once more",
    async (_, lose) => {
      const { devices } = fakeBrowser();
      const { grid, host } = fakeGrid();
      await attach(host);
      await settle();
      const [device] = devices;
      if (!device) {
        throw new Error("no device");
      }
      lose(device);
      // In the same task: nothing may be drawn in between.
      await Promise.resolve();
      await Promise.resolve();
      expect(grid.renderer).toBeNull();
      expect(grid.swaps).toBe(2);
      expect(grid.reports.at(-1)).toBe("device-lost");
      expect(device.destroyed).toBe(true);

      await vi.advanceTimersByTimeAsync(2000);
      expect(grid.renderer?.kind).toBe("webgpu");
      expect(devices).toHaveLength(2);
      if (devices[1]) {
        lose(devices[1]);
      }
      await vi.advanceTimersByTimeAsync(5000);
      expect(grid.renderer).toBeNull();
      expect(grid.reports.at(-1)).toBe("device-lost");
      expect(devices).toHaveLength(2);
    },
  );

  test("counts as lost with the canvas that texts are drawn on", async () => {
    const { canvases, devices } = fakeBrowser();
    const { grid, host } = fakeGrid();
    await attach(host);
    await settle();
    expect(grid.renderer?.kind).toBe("webgpu");
    for (const canvas of canvases) {
      canvas.dispatchEvent(new Event("contextlost"));
    }
    expect(grid.renderer).toBeNull();
    expect(devices[0]?.destroyed).toBe(true);
    await vi.advanceTimersByTimeAsync(2000);
    expect(grid.renderer?.kind).toBe("webgpu");
  });

  test("is not asked for again once that was called off", async () => {
    const { devices } = fakeBrowser();
    const { host } = fakeGrid();
    const stop = await attach(host);
    await settle();
    devices[0]?.destroy();
    await settle();
    stop();
    await vi.advanceTimersByTimeAsync(5000);
    expect(devices).toHaveLength(1);
  });

  test("that has no memory left counts as lost", async () => {
    const { devices } = fakeBrowser();
    const { grid, host } = fakeGrid();
    // The atlas is allocated first, and then the first page's quads.
    vi.spyOn(FakeDevice.prototype, "popErrorScope")
      .mockResolvedValueOnce(null)
      .mockResolvedValueOnce({ message: "" } as never);
    await attach(host);
    await settle();
    expect(grid.reports.at(-1)).toBe("device-lost");
    expect(grid.renderer).toBeNull();
    expect(devices[0]?.destroyed).toBe(true);
  });

  test("lost while it was not yet drawn with is asked for again", async () => {
    const { devices } = fakeBrowser();
    const { grid, host } = fakeGrid(null);
    await attach(host);
    await settle();
    devices[0]?.destroy();
    await settle();
    expect(grid.reports).toEqual(["device-lost"]);
    grid.frame = frameOf({ cache: cacheOf({}, 1, 1) });
    await vi.advanceTimersByTimeAsync(2000);
    expect(grid.renderer?.kind).toBe("webgpu");
    expect(grid.swaps).toBe(1);
  });
});

test("in forced colors a grid is drawn on a 2D canvas", async () => {
  const { devices, media } = fakeBrowser();
  const { grid, host } = fakeGrid();
  media.matches = true;
  const stop = await attach(host);
  await settle();
  expect(grid.reports).toEqual(["forced-colors"]);
  expect(devices).toHaveLength(0);
  media.matches = false;
  media.dispatchEvent(new Event("change"));
  await settle();
  expect(grid.renderer?.kind).toBe("webgpu");
  media.matches = true;
  media.dispatchEvent(new Event("change"));
  expect(grid.renderer).toBeNull();
  expect(devices[0]?.destroyed).toBe(true);
  expect(grid.reports.at(-1)).toBe("forced-colors");
  // Also when it was about to be drawn with WebGPU.
  media.matches = false;
  grid.frame = null;
  media.dispatchEvent(new Event("change"));
  await settle();
  expect(devices.map(({ destroyed }) => destroyed)).toEqual([true, false]);
  media.matches = true;
  media.dispatchEvent(new Event("change"));
  expect(devices.map(({ destroyed }) => destroyed)).toEqual([true, true]);
  expect(grid.renderer).toBeNull();
  stop();
  media.matches = false;
  media.dispatchEvent(new Event("change"));
  await settle();
  expect(grid.renderer).toBeNull();
});
