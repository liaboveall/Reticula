import {
  type Browser,
  type BrowserContextOptions,
  chromium,
  expect,
  type Page,
  test,
} from "@playwright/test";

// The grid's canvas on screens whose pixels are smaller than CSS pixels. Its
// backing store must reach the screen pixel for pixel: scaled by even a
// fraction of a pixel, the grid's hairlines blur. Emulating a device pixel
// ratio does not show that, so the browser itself is started with one.

test.skip(({ browserName }) => browserName !== "chromium", "a switch of Chromium's is used");

const ADDRESS = "http://localhost:5274/app?hooks";
/** Takes back the project's device, which a viewport cannot be without. */
const NOT_EMULATED = Object.assign<BrowserContextOptions, object>(
  { viewport: null },
  { deviceScaleFactor: undefined, isMobile: undefined },
);
/** Offsets of the grid's host that put its corner between device pixels. */
const OFFSETS = [0, 1, 2, 3, 5, 0.4] as const;
/** The part of the canvas that is compared, in device pixels from its corner. */
const PATCH = { left: 40, top: 40, width: 400, height: 240 };

/** Draws a checkerboard of single device pixels over the whole canvas. */
function drawCheckerboard(page: Page): Promise<{ left: number; top: number }> {
  return page.evaluate(() => {
    const canvas = document.querySelector<HTMLCanvasElement>('[data-testid="grid"] canvas');
    const context = canvas?.getContext("2d");
    if (!canvas || !context) {
      throw new Error("the grid has no 2D canvas");
    }
    const { width, height } = canvas;
    const image = context.createImageData(width, height);
    new Uint32Array(image.data.buffer).forEach((_, index, pixels) => {
      const light = ((index % width) + Math.floor(index / width)) % 2 === 0;
      pixels[index] = light ? 0xff_ff_ff_ff : 0xff_00_00_00;
    });
    context.putImageData(image, 0, 0);
    const { left, top } = canvas.getBoundingClientRect();
    const scale = window.devicePixelRatio;
    return { left: Math.round(left * scale), top: Math.round(top * scale) };
  });
}

/** Pixels of the patch that are neither black nor white, or equal to the one before them. */
async function wrongPixels(page: Page, corner: { left: number; top: number }): Promise<number> {
  const png = (await page.screenshot()).toString("base64");
  return page.evaluate(
    async ({ png, left, top, width, height }) => {
      const blob = await (await fetch(`data:image/png;base64,${png}`)).blob();
      const bitmap = await createImageBitmap(blob);
      const context = new OffscreenCanvas(bitmap.width, bitmap.height).getContext("2d");
      if (!context) {
        throw new Error("no 2D context");
      }
      context.drawImage(bitmap, 0, 0);
      const { data } = context.getImageData(left, top, width, height);
      let wrong = 0;
      for (let y = 0; y < height; y++) {
        for (let x = 0; x < width; x++) {
          const at = (y * width + x) * 4;
          const value = data[at];
          if ((value !== 0 && value !== 255) || (x > 0 && value === data[at - 4])) {
            wrong++;
          }
        }
      }
      return wrong;
    },
    {
      png,
      left: corner.left + PATCH.left,
      top: corner.top + PATCH.top,
      width: PATCH.width,
      height: PATCH.height,
    },
  );
}

for (const scale of [1.25, 1.5]) {
  test(`the canvas is not resampled at a device pixel ratio of ${scale}`, async () => {
    let browser: Browser | undefined;
    try {
      browser = await chromium.launch({
        args: [`--force-device-scale-factor=${scale}`, "--window-size=1101,801"],
      });
      const page = await browser.newPage(NOT_EMULATED);
      await page.goto(ADDRESS);
      await expect(page.getByTestId("engine-status")).toHaveText("ready");
      expect(await page.evaluate(() => window.devicePixelRatio)).toBeCloseTo(scale);

      for (const offset of OFFSETS) {
        const before = await page.evaluate(() => window.__reticula?.stats().frames ?? 0);
        await page.getByTestId("grid").evaluate((host, offset) => {
          host.style.marginLeft = `${offset}px`;
          host.style.marginRight = `${offset * 2}px`;
          host.style.marginTop = `${offset}px`;
        }, offset);
        if (offset !== 0) {
          await page.waitForFunction(
            (frames) => (window.__reticula?.stats().frames ?? 0) > frames,
            before,
          );
        }
        await page.waitForFunction(() => window.__reticula?.stats().missingCells === 0);
        expect(await wrongPixels(page, await drawCheckerboard(page)), `${offset} px`).toBe(0);
      }
    } finally {
      await browser?.close();
    }
  });
}
