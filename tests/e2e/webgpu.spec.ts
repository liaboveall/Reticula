import { type Browser, expect, type Page, test } from "@playwright/test";
import { fillFormulaBar } from "./formula-bar-helpers.ts";

// The grid drawn with WebGPU in the app. What it draws is compared with what a
// 2D canvas draws of the same state, pixel by pixel.

declare global {
  interface Window {
    /** Devices the page has been given, and how many of them are lost or given back. */
    __gpu?: { readonly devices: GPUDevice[]; gone: number };
  }
}

/** Levels of a channel: text that is drawn whole, or over something translucent, is rounded twice. */
const TOLERANCE = 4;

let complaints: string[] = [];

test.beforeEach(async ({ page }) => {
  complaints = [];
  page.on("pageerror", (error) => complaints.push(error.message));
  await watchDevices(page);
});

test.afterEach(() => {
  expect(complaints).toEqual([]);
});

async function watchDevices(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const gpu = { devices: [] as GPUDevice[], gone: 0 };
    window.__gpu = gpu;
    const request = GPUAdapter.prototype.requestDevice;
    GPUAdapter.prototype.requestDevice = async function (descriptor) {
      const device = await request.call(this, descriptor);
      gpu.devices.push(device);
      void device.lost.then(() => {
        gpu.gone++;
      });
      return device;
    };
  });
}

async function open(page: Page, query: string): Promise<void> {
  await page.goto(`/app?hooks${query}`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await page.waitForFunction(() => (window.__reticula?.stats().cellsDrawn ?? 0) > 0);
  await settled(page);
}

/** Waits until a frame has been drawn with everything in it. */
async function settled(page: Page): Promise<void> {
  await nextFrames(page);
  await page.waitForFunction(() => {
    const stats = window.__reticula?.stats();
    return stats !== undefined && stats.pagesHeld > 0 && stats.missingCells === 0;
  });
  await nextFrames(page);
}

function nextFrames(page: Page): Promise<void> {
  return page.evaluate(
    () =>
      new Promise<void>((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
      ),
  );
}

async function drawnWith(page: Page, kind: "canvas2d" | "webgpu"): Promise<void> {
  await page.waitForFunction((kind) => window.__reticula?.stats().renderer.kind === kind, kind);
  // The renderer it had before is there until this one has drawn.
  await expect(page.locator(".reticula-grid canvas")).toHaveCount(1);
}

function stats(page: Page) {
  return page.evaluate(() => {
    const hooks = window.__reticula;
    if (!hooks) {
      throw new Error("the test hooks are not installed");
    }
    return hooks.stats();
  });
}

async function counters(page: Page) {
  const { renderer } = await stats(page);
  if (!renderer.counters) {
    throw new Error(`no counters: ${JSON.stringify(renderer)}`);
  }
  return renderer.counters;
}

/** Selects a cell, so that a frame is drawn, and reads that frame back. */
async function picture(page: Page, row: number, col: number): Promise<Buffer> {
  const pixels = await page.evaluate(
    ([row, col]) =>
      new Promise<string>((resolve, reject) => {
        window.__reticula?.select(row, col);
        // After the grid's own callback, which selecting has requested.
        requestAnimationFrame(() => {
          const canvas = [
            ...document.querySelectorAll<HTMLCanvasElement>(".reticula-grid canvas"),
          ].at(-1);
          const copy = document.createElement("canvas");
          copy.width = canvas?.width ?? 0;
          copy.height = canvas?.height ?? 0;
          const context = copy.getContext("2d");
          if (!canvas || !context) {
            reject(new Error("there is no canvas to read"));
            return;
          }
          context.drawImage(canvas, 0, 0);
          const bytes = context.getImageData(0, 0, copy.width, copy.height).data;
          let text = "";
          for (let at = 0; at < bytes.length; at += 0x8000) {
            text += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
          }
          resolve(btoa(text));
        });
      }),
    [row, col] as const,
  );
  return Buffer.from(pixels, "base64");
}

/** How far apart two pictures are, in levels of a channel. */
function apart(a: Buffer, b: Buffer): number {
  if (a.length !== b.length || a.length === 0) {
    return Number.POSITIVE_INFINITY;
  }
  let most = 0;
  for (const [at, value] of a.entries()) {
    most = Math.max(most, Math.abs(value - (b[at] ?? 0)));
  }
  return most;
}

/** Pixels that do not look like the one in the middle of the corner. */
function drawnOn(pixels: Buffer): number {
  let count = 0;
  for (let at = 0; at < pixels.length; at += 4) {
    count += pixels.readUInt32LE(at) === pixels.readUInt32LE(0) ? 0 : 1;
  }
  return count;
}

/** A page whose grid is drawn on a 2D canvas, with storage of its own. */
async function openReference(browser: Browser): Promise<Page> {
  const page = await browser.newPage();
  await open(page, "&renderer=canvas2d");
  return page;
}

/** Destroys the device behind the app's back, and says what the app does in the next frame. */
function loseDevice(page: Page) {
  return page.evaluate(async () => {
    const device = window.__gpu?.devices.at(-1);
    const hooks = window.__reticula;
    if (!device || !hooks) {
      throw new Error("there is no device");
    }
    const before = hooks.stats().frames;
    device.destroy();
    await device.lost;
    const { renderer } = hooks.stats();
    await new Promise((resolve) => requestAnimationFrame(resolve));
    return { renderer, frames: hooks.stats().frames - before };
  });
}

test("the grid is drawn with WebGPU once the first screen is there, and looks the same", async ({
  page,
  browser,
}) => {
  await open(page, "&renderer=webgpu");
  await drawnWith(page, "webgpu");
  const { renderer } = await stats(page);
  expect(renderer.reason).toBeNull();
  expect(renderer.adapter).toMatch(/swiftshader/i);

  const loaded = await page.evaluate(() => ({
    firstScreen: performance.getEntriesByName("reticula:first-screen")[0]?.startTime ?? 0,
    chunk:
      performance
        .getEntriesByType("resource")
        .find((entry) => /\/webgpu-[^/]+\.js$/.test(entry.name))?.startTime ?? 0,
  }));
  expect(loaded.firstScreen).toBeGreaterThan(0);
  expect(loaded.chunk).toBeGreaterThan(loaded.firstScreen);

  // Nothing had to be left out of the first frame: what it needed was there.
  const { frames, deferredFrames } = await counters(page);
  expect(frames).toBeGreaterThan(0);
  const drawn = await picture(page, 3, 1);
  expect(drawnOn(drawn)).toBeGreaterThan(5_000);
  expect(apart(drawn, await picture(await openReference(browser), 3, 1))).toBeLessThanOrEqual(
    TOLERANCE,
  );
  expect((await counters(page)).deferredFrames).toBe(deferredFrames);
});

test("whatever is done to the sheet looks the same", async ({ page, browser }) => {
  test.setTimeout(90_000);
  const pages = [page, await openReference(browser)] as const;
  await open(page, "&renderer=webgpu");
  await drawnWith(page, "webgpu");

  const expectSame = async (label: string, row: number, col: number) => {
    await Promise.all(pages.map(settled));
    const [drawn, wanted] = await Promise.all(pages.map((page) => picture(page, row, col)));
    expect((await stats(page)).renderer.kind, label).toBe("webgpu");
    expect(apart(drawn ?? Buffer.of(), wanted ?? Buffer.of()), label).toBeLessThanOrEqual(
      TOLERANCE,
    );
  };
  const onEach = (act: (page: Page) => Promise<unknown>) => Promise.all(pages.map(act));
  const scrollTo = (top: number, left: number) =>
    onEach((page) =>
      page.evaluate(([top, left]) => window.__reticula?.scrollTo({ top, left }), [
        top,
        left,
      ] as const),
    );

  await scrollTo(24 * 500_000 + 7, 33);
  await expectSame("half a million rows down", 500_003, 2);

  await scrollTo(0, 0);
  await onEach((page) =>
    page.evaluate(() => window.__reticula?.selectRange({ row: 1, col: 1 }, { row: 5, col: 3 })),
  );
  await expectSame("after a selection", 2, 0);

  await onEach(async (page) => {
    await page.evaluate(() => window.__reticula?.select(4, 2));
    await expect(page.getByTestId("name-box")).toHaveText("C5");
    await fillFormulaBar(page, "Grüße 你好 שלום");
    await page.waitForFunction(() => window.__reticula?.textAt(4, 2) === "Grüße 你好 שלום");
  });
  await expectSame("after an edit", 6, 1);

  await onEach((page) => page.getByTestId("freeze-select").selectOption("row1col1"));
  await scrollTo(24 * 300 + 5, 112 * 3 + 40);
  await expectSame("with frozen panes", 310, 5);

  await onEach(async (page) => {
    await page.getByTestId("zoom-value").click();
    await page.getByTestId("zoom-50").click();
    await page.waitForFunction(() => window.__reticula?.stats().zoom === 0.5);
  });
  await expectSame("zoomed out", 305, 6);

  await onEach((page) => page.emulateMedia({ colorScheme: "dark" }));
  await expectSame("in the dark theme", 306, 6);
});

test("scrolling within what has been drawn uploads nothing but the frame's own quads", async ({
  page,
}) => {
  await open(page, "&renderer=webgpu");
  await drawnWith(page, "webgpu");
  const scrollTo = async (top: number) => {
    await page.evaluate((top) => window.__reticula?.scrollTo({ top }), top);
    await settled(page);
  };
  await scrollTo(90);
  await scrollTo(0);
  const before = await counters(page);
  for (const top of [13, 40, 71, 90, 22, 0]) {
    await scrollTo(top);
  }
  const after = await counters(page);
  expect(after.frames).toBeGreaterThanOrEqual(before.frames + 6);
  expect(after.pageUploads).toBe(before.pageUploads);
  expect(after.bandsBuilt).toBe(before.bandsBuilt);
  expect(after.atlasUploads).toBe(before.atlasUploads);
});

test("when the device is lost, the next frame is drawn on a 2D canvas", async ({
  page,
  browser,
}) => {
  await open(page, "&renderer=webgpu");
  await drawnWith(page, "webgpu");
  const wanted = await picture(await openReference(browser), 3, 1);

  const lost = await loseDevice(page);
  expect(lost.renderer).toEqual({
    kind: "canvas2d",
    reason: "device-lost",
    adapter: null,
    counters: null,
  });
  expect(lost.frames).toBe(1);
  expect(apart(await picture(page, 3, 1), wanted)).toBe(0);

  // It is tried again, but not a third time.
  await drawnWith(page, "webgpu");
  expect(apart(await picture(page, 3, 1), wanted)).toBeLessThanOrEqual(TOLERANCE);
  expect((await loseDevice(page)).renderer.kind).toBe("canvas2d");
  await page.waitForTimeout(2_500);
  expect((await stats(page)).renderer).toMatchObject({ kind: "canvas2d", reason: "device-lost" });
  expect(await page.evaluate(() => window.__gpu?.devices.length)).toBe(2);
  expect(apart(await picture(page, 3, 1), wanted)).toBe(0);
});

test("the sheet is still there when the GPU process is not", async ({ page, browser }) => {
  await open(page, "&renderer=webgpu");
  await drawnWith(page, "webgpu");
  const wanted = await picture(await openReference(browser), 3, 1);
  const session = await browser.newBrowserCDPSession();
  await session.send("Browser.crashGpuProcess").catch(() => {
    test.skip(true, "there is no GPU process to crash");
  });
  await page.waitForFunction(() => (window.__gpu?.gone ?? 0) > 0);
  // Both the device and the 2D canvas that took over may have been lost with it.
  await expect(async () => {
    await settled(page);
    expect(apart(await picture(page, 3, 1), wanted)).toBeLessThanOrEqual(TOLERANCE);
  }).toPass();
});

test("two grids share a device, and leave none behind", async ({ page }) => {
  await page.goto("/sim?renderer=webgpu");
  const onGpu = () =>
    page.evaluate(
      () =>
        [...document.querySelectorAll<HTMLCanvasElement>(".reticula-grid canvas")].filter(
          // A canvas has one kind of context.
          (canvas) => canvas.getContext("2d") === null,
        ).length,
    );
  await expect.poll(onGpu, { timeout: 15_000 }).toBe(2);
  expect(await page.evaluate(() => window.__gpu?.devices.length)).toBe(1);

  await page.evaluate(() => {
    history.pushState(null, "", "/");
    window.dispatchEvent(new PopStateEvent("popstate"));
  });
  await expect(page.getByTestId("landing-page")).toBeVisible();
  await page.waitForFunction(() => window.__gpu?.gone === window.__gpu?.devices.length);
});

test("an editor that is closed leaves no device behind", async ({ page }) => {
  await open(page, "&renderer=webgpu");
  await drawnWith(page, "webgpu");
  await page.evaluate(() => {
    history.pushState(null, "", "/");
    window.dispatchEvent(new PopStateEvent("popstate"));
  });
  await expect(page.getByTestId("landing-page")).toBeVisible();
  await page.waitForFunction(() => window.__gpu?.gone === 1);
  expect(await page.evaluate(() => window.__gpu?.devices.length)).toBe(1);
});

test("a software adapter is not used unless it is asked for", async ({ page }) => {
  await open(page, "");
  await page.waitForFunction(
    () => window.__reticula?.stats().renderer.reason === "software-adapter",
  );
  expect((await stats(page)).renderer.kind).toBe("canvas2d");
  expect(await page.evaluate(() => window.__gpu?.devices.length)).toBe(0);
});

test("who chooses Canvas 2D gets it, now and later", async ({ page }) => {
  await open(page, "&renderer=webgpu");
  await drawnWith(page, "webgpu");
  await page.evaluate(() => window.__reticula?.setRenderer("canvas2d"));
  await drawnWith(page, "canvas2d");
  expect((await stats(page)).renderer.reason).toBe("preference");
  await page.waitForFunction(() => window.__gpu?.gone === 1);

  await open(page, "");
  await page.waitForTimeout(500);
  expect((await stats(page)).renderer).toMatchObject({ kind: "canvas2d", reason: "preference" });
});

test("in forced colors the grid is drawn on a 2D canvas", async ({ page }) => {
  await open(page, "&renderer=webgpu");
  await drawnWith(page, "webgpu");
  await page.emulateMedia({ forcedColors: "active" });
  await drawnWith(page, "canvas2d");
  expect((await stats(page)).renderer.reason).toBe("forced-colors");
  await page.emulateMedia({ forcedColors: "none" });
  await drawnWith(page, "webgpu");
});
