import { readFileSync } from "node:fs";
import { expect, type Page, test } from "@playwright/test";

// The same value the native and Node runs are checked against. Each browser
// has its own WebAssembly implementation; all of them must agree with it.
const expectedDigest = readFileSync(
  new URL("../determinism/expected.txt", import.meta.url),
  "utf8",
).trim();

const ROWS = 1_000_000;
const COLS = 22;

/** Opens the app with its test hooks and waits until the sample is on screen. */
async function open(page: Page): Promise<void> {
  await page.goto("/app?hooks");
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await settled(page);
}

/** Waits until a frame has been drawn in which every visible cell had its data. */
async function settled(page: Page): Promise<void> {
  await page.waitForFunction(() => {
    const stats = window.__reticula?.stats();
    return stats !== undefined && stats.cellsDrawn > 0 && stats.missingCells === 0;
  });
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

function textAt(page: Page, row: number, col: number) {
  return page.evaluate(([row, col]) => window.__reticula?.textAt(row, col) ?? null, [
    row,
    col,
  ] as const);
}

test("the engine generates the sample in a worker and the grid draws it", async ({ page }) => {
  await open(page);
  await expect(page.getByTestId("document")).toContainText("1,000,000 rows × 22 columns");
  await expect(page.getByTestId("kernel-version")).toHaveText(/^kernel \d+\.\d+\.\d+/);

  const first = await stats(page);
  expect(first.sheet).toEqual({ rows: ROWS, cols: COLS });
  expect(first.visible.firstRow).toBe(0);
  expect(first.visible.firstCol).toBe(0);
  expect(first.cellsDrawn).toBeGreaterThan(0);
  expect(first.missingCells).toBe(0);
  // The first row names the columns; the first order follows.
  expect(await textAt(page, 0, 0)).toBe("订单号");
  expect(await textAt(page, 1, 0)).toBe("10000000");
  expect(await textAt(page, 1, 1)).toBe("2023-01-01");
});

test("the page marks the moments of starting up, in order", async ({ page }) => {
  await open(page);
  const marks = await page.evaluate(() =>
    Object.fromEntries(
      performance.getEntriesByType("mark").map((mark) => [mark.name, mark.startTime]),
    ),
  );
  const shell = marks["reticula:shell"] ?? Number.NaN;
  const ready = marks["reticula:engine-ready"] ?? Number.NaN;
  const firstScreen = marks["reticula:first-screen"] ?? Number.NaN;
  expect(shell).toBeGreaterThan(0);
  // The grid is on screen before the engine has anything to show.
  expect(ready).toBeGreaterThan(shell);
  expect(firstScreen).toBeGreaterThan(ready);
});

test("the engine passes the determinism self-check in this browser", async ({ page }) => {
  await open(page);
  const digest = await page.evaluate(() => window.__reticula?.selfCheck());
  expect(digest).toBe(expectedDigest);
});

test("scrolling to the end shows the last row and column", async ({ page }) => {
  await open(page);
  const before = await stats(page);

  await page.evaluate(() =>
    window.__reticula?.scrollTo({ top: Number.MAX_SAFE_INTEGER, left: Number.MAX_SAFE_INTEGER }),
  );
  await page.waitForFunction((rows) => window.__reticula?.stats().visible.endRow === rows, ROWS);
  await settled(page);

  const after = await stats(page);
  expect(after.visible.endRow).toBe(ROWS);
  expect(after.visible.endCol).toBe(COLS);
  expect(after.visible.firstRow).toBeGreaterThan(before.visible.endRow);
  expect(after.scroll).toEqual(after.scrollLimits);
  // The last order: number, date and one of the computed amounts.
  expect(await textAt(page, ROWS - 1, 0)).toBe(String(10_000_000 + ROWS - 2));
  expect(await textAt(page, ROWS - 1, 1)).toBe("2025-12-31");
  expect(await textAt(page, ROWS - 1, 12)).toMatch(/^\d+\.\d\d$/);
  // The pages of the first screen were dropped when the grid moved away.
  expect(await textAt(page, 0, 0)).toBeNull();
});

test("the wheel scrolls and a click selects the cell under the pointer", async ({ page }) => {
  await open(page);
  const grid = page.getByTestId("grid");
  const box = await grid.boundingBox();
  if (!box) {
    throw new Error("the grid has no box");
  }
  const { metrics } = await stats(page);

  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.wheel(0, metrics.rowHeight * 100);
  await page.waitForFunction(() => (window.__reticula?.stats().visible.firstRow ?? 0) >= 100);
  await settled(page);
  const scrolled = await stats(page);
  expect(scrolled.visible.firstRow).toBe(100);
  expect(scrolled.scroll.top).toBe(metrics.rowHeight * 100);

  // The third column and fourth row in view, somewhere inside the cell.
  await page.mouse.click(
    box.x + metrics.headerWidth + metrics.colWidth * 2.5,
    box.y + metrics.headerHeight + metrics.rowHeight * 3.5,
  );
  await page.waitForFunction(() => window.__reticula?.stats().selection !== null);
  const { selection } = await stats(page);
  expect(selection).toMatchObject({ row: 103, col: 2, name: "C104", rowId: "103", colId: "2" });
  expect(selection?.text).toBe(await textAt(page, 103, 2));
  await expect(page.getByTestId("selection")).toContainText("C104");
});

test("the scrollbar's thumb can be dragged and its track pressed", async ({ page }) => {
  await open(page);
  const track = page.locator('.reticula-scrollbar[data-orientation="vertical"]');
  const thumb = track.locator(".reticula-scrollbar-thumb");
  const trackBox = await track.boundingBox();
  const thumbBox = await thumb.boundingBox();
  if (!trackBox || !thumbBox) {
    throw new Error("the vertical scrollbar is not laid out");
  }
  const x = thumbBox.x + thumbBox.width / 2;
  const middle = trackBox.y + trackBox.height / 2;

  await page.mouse.move(x, thumbBox.y + thumbBox.height / 2);
  await page.mouse.down();
  // Half way down the track is half way down the sheet.
  await page.mouse.move(x, middle, { steps: 5 });
  await page.waitForFunction(() => (window.__reticula?.stats().visible.firstRow ?? 0) > 400_000);
  expect((await stats(page)).visible.firstRow).toBeLessThan(600_000);
  // Dragged past the end of the track, the thumb stops at the end. The
  // pointer stays inside the window: a browser driven by a test does not
  // report positions outside it faithfully.
  await page.mouse.move(x, trackBox.y + trackBox.height + 16, { steps: 5 });
  await page.mouse.up();
  await page.waitForFunction((rows) => window.__reticula?.stats().visible.endRow === rows, ROWS);
  await settled(page);
  expect(await textAt(page, ROWS - 1, 1)).toBe("2025-12-31");
  const end = await stats(page);

  // Once released, the thumb no longer follows the pointer.
  await page.mouse.move(x, middle, { steps: 3 });
  await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => done(null))));
  expect((await stats(page)).scroll.top).toBe(end.scroll.top);

  // A press on the track above the thumb goes back by one screenful.
  await page.mouse.click(x, middle);
  await page.waitForFunction(
    (rows) => (window.__reticula?.stats().visible.endRow ?? rows) < rows,
    ROWS,
  );
  await settled(page);
  const paged = await stats(page);
  const screenful = end.visible.endRow - end.visible.firstRow;
  const moved = end.visible.firstRow - paged.visible.firstRow;
  expect(moved).toBeGreaterThanOrEqual(screenful - 2);
  expect(moved).toBeLessThanOrEqual(screenful);
});

test("cells come back after the window was too small to show any", async ({ page }) => {
  await open(page);
  const size = page.viewportSize();
  const box = await page.getByTestId("grid").boundingBox();
  if (!size || !box) {
    throw new Error("the page has no size");
  }
  // Leave the grid a strip that its headers and scrollbar fill completely.
  await page.setViewportSize({
    width: size.width,
    height: Math.round(size.height - box.height) + 20,
  });
  await page.waitForFunction(() => {
    const visible = window.__reticula?.stats().visible;
    return visible !== undefined && visible.endRow === visible.firstRow;
  });
  expect(await textAt(page, 0, 0)).toBeNull();

  await page.setViewportSize(size);
  await settled(page);
  expect(await textAt(page, 0, 0)).toBe("订单号");
});
