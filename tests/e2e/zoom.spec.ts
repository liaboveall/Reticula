import { expect, type Page, test } from "@playwright/test";
import {
  type CollabServerHandle,
  editCell,
  signInDev,
  startCollabServer,
} from "./collab-helpers.ts";

// Zooming the sheet: what is at the pointer stays there, sizes are kept the way
// the workbook has them, and the next visit finds everything as it was left.

async function settled(page: Page): Promise<void> {
  await nextFrames(page);
  await page.waitForFunction(() => {
    const stats = window.__reticula?.stats();
    return stats !== undefined && stats.pagesHeld > 0 && stats.missingCells === 0;
  });
}

async function ready(page: Page): Promise<void> {
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await page.waitForFunction(() => (window.__reticula?.stats().cellsDrawn ?? 0) > 0);
  await settled(page);
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

async function gridBox(page: Page) {
  const box = await page.getByTestId("grid").boundingBox();
  if (!box) {
    throw new Error("the grid has no box");
  }
  return box;
}

/** A point of the page by its distance from the corner of the first cell. */
async function at(page: Page, x: number, y: number) {
  const box = await gridBox(page);
  const { metrics } = await stats(page);
  return { x: box.x + metrics.headerWidth + x, y: box.y + metrics.headerHeight + y };
}

/** Where the middle of a cell in view is, when no column or row has a size of its own. */
async function middleOf(page: Page, row: number, col: number) {
  const { metrics, scroll } = await stats(page);
  return at(
    page,
    (col + 0.5) * metrics.colWidth - scroll.left,
    (row + 0.5) * metrics.rowHeight - scroll.top,
  );
}

/** Where the middle of the header of a column or a row in view is. */
async function columnHeader(page: Page, col: number) {
  const { metrics, scroll } = await stats(page);
  return at(page, (col + 0.5) * metrics.colWidth - scroll.left, -metrics.headerHeight / 2);
}

async function rowHeader(page: Page, row: number) {
  const { metrics, scroll } = await stats(page);
  return at(page, -metrics.headerWidth / 2, (row + 0.5) * metrics.rowHeight - scroll.top);
}

async function click(page: Page, point: { x: number; y: number }) {
  await page.mouse.click(point.x, point.y);
  return (await stats(page)).selection;
}

async function scrollToRow(page: Page, row: number, col = 0): Promise<void> {
  const { metrics } = await stats(page);
  await page.evaluate((to) => window.__reticula?.scrollTo(to), {
    top: row * metrics.rowHeight,
    left: col * metrics.colWidth,
  });
  await page.waitForFunction((row) => window.__reticula?.stats().visible.firstRow === row, row);
  await settled(page);
}

async function zoomIs(page: Page, zoom: number): Promise<void> {
  await page.waitForFunction((zoom) => window.__reticula?.stats().zoom === zoom, zoom);
  await expect(page.getByTestId("zoom-value")).toHaveText(`${Math.round(zoom * 100)}%`);
  await settled(page);
}

async function chooseZoom(page: Page, percent: number): Promise<void> {
  await page.getByTestId("zoom-value").click();
  await page.getByTestId(`zoom-${percent}`).click();
  await zoomIs(page, percent / 100);
}

/** Turns the wheel with the key held, as with a mouse. */
async function turnWheel(page: Page, notches: number): Promise<void> {
  await page.keyboard.down("Control");
  for (let notch = 0; notch < Math.abs(notches); notch++) {
    await page.mouse.wheel(0, notches > 0 ? -100 : 100);
    await nextFrames(page);
  }
  await page.keyboard.up("Control");
}

function nextFrames(page: Page): Promise<void> {
  return page.evaluate(
    () =>
      new Promise<void>((done) => requestAnimationFrame(() => requestAnimationFrame(() => done()))),
  );
}

/** Operations the engine has recorded; asking for its digest makes sure that it has dealt with all of them. */
function operations(page: Page): Promise<number> {
  return page.evaluate(async () => {
    await window.__reticula?.stateDigest();
    return Number(window.__reticula?.docStatus().walSeq ?? "0");
  });
}

test.beforeEach(async ({ page }) => {
  await page.goto("/app?hooks");
  await ready(page);
});

test("the wheel with Ctrl goes through the steps, and the cell at the pointer stays there", async ({
  page,
}) => {
  await scrollToRow(page, 500_000);
  const point = await middleOf(page, 500_011, 4);
  await page.mouse.move(point.x, point.y);
  const density = await page.evaluate(() => window.devicePixelRatio);
  let zoom = 1;
  for (const [notches, next] of [
    [1, 1.1],
    [-4, 0.75],
    [-2, 0.5],
    [4, 0.9],
    [1, 1],
  ] as const) {
    await turnWheel(page, notches);
    zoom = next;
    await zoomIs(page, zoom);
    // From side to side the whole sheet comes into view, and then there is nothing to scroll.
    expect((await click(page, point))?.row).toBe(500_011);
  }
  expect(await page.evaluate(() => window.devicePixelRatio)).toBe(density);
});

test("the status bar has the steps, and leaves the keyboard to the grid", async ({ page }) => {
  await click(page, await middleOf(page, 3, 2));
  await page.getByTestId("zoom-in").click();
  await zoomIs(page, 1.1);
  await page.getByTestId("zoom-out").click();
  await page.getByTestId("zoom-out").click();
  await zoomIs(page, 0.9);
  await page.keyboard.press("ArrowDown");
  expect((await stats(page)).selection).toMatchObject({ row: 4, col: 2 });

  await chooseZoom(page, 200);
  await expect(page.getByTestId("zoom-in")).toHaveAttribute("aria-disabled", "true");
  await expect(page.getByTestId("zoom-menu")).toHaveCount(0);
  await page.keyboard.press("ArrowRight");
  expect((await stats(page)).selection).toMatchObject({ row: 4, col: 3 });

  // The keyboard alone will do.
  await page.getByTestId("zoom-value").focus();
  await page.keyboard.press("Enter");
  await expect(page.getByRole("menuitemradio", { name: "200%" })).toBeFocused();
  await page.keyboard.press("Home");
  await expect(page.getByRole("menuitemradio", { name: "25%", exact: true })).toBeFocused();
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");
  await zoomIs(page, 0.5);
  await expect(page.getByTestId("zoom-value")).toBeFocused();
  await page.keyboard.press("Space");
  await expect(page.getByTestId("zoom-menu")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("zoom-menu")).toHaveCount(0);
  await zoomIs(page, 0.5);
});

test("the next visit finds zoom, place and selection as they were left", async ({ page }) => {
  await chooseZoom(page, 50);
  await scrollToRow(page, 500_000, 2);
  const cell = await middleOf(page, 500_007, 5);
  await click(page, cell);
  await page.keyboard.press("Shift+ArrowDown");
  const before = await stats(page);
  expect(before.selection?.name).toBe("F500008:F500009");

  await page.reload();
  await ready(page);
  await zoomIs(page, 0.5);
  const after = await stats(page);
  expect(after.selection?.name).toBe("F500008:F500009");
  expect(after.visible).toEqual(before.visible);
  expect(after.metrics).toEqual(before.metrics);
});

test.describe("in detail", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "the rules are the same everywhere");

  test("the middle of a cell and of a header is theirs at every step", async ({ page }) => {
    for (const percent of [25, 33, 50, 67, 75, 80, 90, 110, 125, 150, 175, 200]) {
      await chooseZoom(page, percent);
      const { firstRow, firstCol } = (await stats(page)).visible;
      for (const [down, right] of [
        [1, 1],
        [2, 1],
        [3, 2],
      ] as const) {
        const cell = { row: firstRow + down, col: firstCol + right };
        const middle = await middleOf(page, cell.row, cell.col);
        expect(await click(page, middle)).toMatchObject({ ...cell, scope: "cells" });
      }
      const column = await click(page, await columnHeader(page, firstCol + 2));
      expect(column).toMatchObject({ scope: "cols", col: firstCol + 2 });
      const row = await click(page, await rowHeader(page, firstRow + 3));
      expect(row).toMatchObject({ scope: "rows", row: firstRow + 3 });
    }
  });

  test("a pinch stops at multiples of five per cent, with one change a frame", async ({ page }) => {
    await scrollToRow(page, 500_000);
    const point = await middleOf(page, 500_008, 3);
    const during = await page.evaluate(async ({ x, y }) => {
      const hooks = window.__reticula;
      const input = document.querySelector(".reticula-input");
      if (!hooks || !input) {
        throw new Error("the grid is not there");
      }
      const pinch = (deltaY: number) => {
        const event = new WheelEvent("wheel", {
          deltaY,
          ctrlKey: true,
          clientX: x,
          clientY: y,
          bubbles: true,
          cancelable: true,
        });
        input.dispatchEvent(event);
        return event.defaultPrevented;
      };
      const frame = () => new Promise(requestAnimationFrame);
      const prevented = pinch(0.4);
      const zooms: number[] = [];
      for (let round = 0; round < 12; round++) {
        const { zoom } = hooks.stats();
        for (let event = 0; event < 5; event++) {
          pinch(1.1);
          if (hooks.stats().zoom !== zoom) {
            throw new Error("the zoom has changed between frames");
          }
        }
        await frame();
        await frame();
        zooms.push(hooks.stats().zoom);
      }
      return { prevented, zooms };
    }, point);
    expect(during.prevented).toBe(true);
    const { zooms } = during;
    expect([...zooms].sort((a, b) => b - a)).toEqual(zooms);
    expect(zooms.at(-1)).toBeCloseTo(0.5, 9);
    for (const zoom of zooms) {
      expect(zoom * 20).toBeCloseTo(Math.round(zoom * 20), 6);
    }
    await page.mouse.move(point.x, point.y);
    expect((await click(page, point))?.row).toBe(500_008);
  });

  test("pages that are far out of view are let go of when the zoom has come to rest", async ({
    page,
  }) => {
    await scrollToRow(page, 500_000);
    await chooseZoom(page, 25);
    await settled(page);
    const held = (await stats(page)).pagesHeld;
    const point = await middleOf(page, 500_011, 4);
    await page.mouse.move(point.x, point.y);
    await turnWheel(page, 12);
    await zoomIs(page, 2);
    await page.waitForFunction(
      (held) => (window.__reticula?.stats().pagesHeld ?? held) < held / 2,
      held,
    );
    await settled(page);
  });

  test("a change of the zoom is not taken for fast scrolling", async ({ page }) => {
    const nudge = () =>
      page.evaluate(() => {
        const hooks = window.__reticula;
        hooks?.scrollTo({ top: hooks.stats().scroll.top + 1 });
      });
    await scrollToRow(page, 500_000);
    // Longer than fast scrolling is remembered.
    await page.waitForTimeout(600);
    await chooseZoom(page, 50);
    await nudge();
    await settled(page);
    const held = (await stats(page)).pagesHeld;
    await page.waitForTimeout(600);
    // Holds what is wanted at rest, and nothing else.
    await chooseZoom(page, 50);
    await nudge();
    await settled(page);
    expect(held).toBe((await stats(page)).pagesHeld);
  });

  test("sizes are kept the way the workbook has them, whatever the zoom", async ({ page }) => {
    await chooseZoom(page, 50);
    const { metrics } = await stats(page);
    expect(metrics).toMatchObject({ rowHeight: 12, colWidth: 56, headerHeight: 18 });
    const before = await operations(page);

    // Pressed and let go, an edge does nothing.
    const edge = await at(page, metrics.colWidth, -9);
    const rowEdge = await at(page, -20, metrics.rowHeight * 3 - 1);
    for (const { x, y } of [edge, rowEdge]) {
      await page.mouse.move(x, y);
      await page.mouse.down();
      await page.mouse.up();
    }
    expect(await operations(page)).toBe(before);

    await page.mouse.move(edge.x, edge.y);
    await page.mouse.down();
    await page.mouse.move(edge.x + 40, edge.y, { steps: 4 });
    await page.mouse.up();
    await page.mouse.move(rowEdge.x, rowEdge.y);
    await page.mouse.down();
    await page.mouse.move(rowEdge.x, rowEdge.y + 13, { steps: 4 });
    await page.mouse.up();
    await expect.poll(() => operations(page)).toBe(before + 2);

    // What the engine has kept: 112 + 2 × 40 and 24 + 2 × 13.
    await page.reload();
    await ready(page);
    await chooseZoom(page, 100);
    expect((await click(page, await at(page, 186, -10)))?.range.firstCol).toBe(0);
    expect((await click(page, await at(page, 198, -10)))?.range.firstCol).toBe(1);
    expect((await click(page, await at(page, -20, 48 + 44)))?.range.firstRow).toBe(2);
    expect((await click(page, await at(page, -20, 48 + 56)))?.range.firstRow).toBe(3);
  });

  test("a column is made to fit the same at every zoom", async ({ page }) => {
    await chooseZoom(page, 50);
    const before = await operations(page);
    const edge = await at(page, 56, -9);
    await page.mouse.dblclick(edge.x, edge.y);
    await expect.poll(() => operations(page)).toBe(before + 1);
    // It fits already: at another zoom there is nothing to change.
    await page.mouse.move(0, 0);
    await chooseZoom(page, 200);
    const { metrics } = await stats(page);
    let width = 0;
    for (let x = 4; x < metrics.colWidth * 2; x += 2) {
      const cell = await at(page, x, metrics.rowHeight / 2);
      if ((await click(page, cell))?.col === 1) {
        width = x;
        break;
      }
    }
    expect(width).toBeGreaterThan(96);
    const wider = await at(page, width - 1, -18);
    await page.mouse.dblclick(wider.x, wider.y);
    await nextFrames(page);
    expect(await operations(page)).toBe(before + 1);
  });

  test("nothing zooms while something is dragged", async ({ page }) => {
    const from = await middleOf(page, 2, 1);
    const to = await middleOf(page, 6, 3);
    await page.mouse.move(from.x, from.y);
    await page.mouse.down();
    await page.mouse.move(to.x, to.y, { steps: 3 });
    await turnWheel(page, -3);
    expect((await stats(page)).zoom).toBe(1);
    await page.mouse.up();
    expect((await stats(page)).selection?.name).toBe("B3:D7");
    await turnWheel(page, -1);
    await zoomIs(page, 0.9);
  });

  test("at another density the first row in view is the same", async ({ page }) => {
    const session = await page.context().newCDPSession(page);
    const { width, height } = page.viewportSize() ?? { width: 1280, height: 720 };
    for (const percent of [100, 33, 175]) {
      await chooseZoom(page, percent);
      await scrollToRow(page, 500_000);
      for (const deviceScaleFactor of [1.1, 0.9, 1.25, 2, 1]) {
        await session.send("Emulation.setDeviceMetricsOverride", {
          width: Math.round(width / deviceScaleFactor),
          height: Math.round(height / deviceScaleFactor),
          deviceScaleFactor,
          mobile: false,
        });
        await page.waitForFunction(
          (wanted) => Math.abs(window.devicePixelRatio - wanted) < 1e-3,
          deviceScaleFactor,
        );
        await nextFrames(page);
        await settled(page);
        const { visible, metrics } = await stats(page);
        expect(visible.firstRow, `${percent}% at ${deviceScaleFactor}`).toBe(500_000);
        const density = await page.evaluate(() => window.devicePixelRatio);
        for (const size of [metrics.rowHeight, metrics.colWidth, metrics.headerHeight]) {
          const pixels = size * density;
          expect(Math.abs(pixels - Math.round(pixels))).toBeLessThan(1e-6);
        }
      }
    }
  });

  test("frozen panes stay where they are", async ({ page }) => {
    await page.getByTestId("freeze-select").click();
    await page.getByTestId("freeze-row1col1").click();
    await nextFrames(page);
    await scrollToRowUnderHeader(page, 500_000);
    for (const notches of [-4, 7, -3]) {
      // Over a frozen cell there is nothing to stay at but what scrolls next to it.
      const { metrics } = await stats(page);
      const frozen = await at(page, metrics.colWidth * 3.5, 2);
      await page.mouse.move(frozen.x, frozen.y);
      await turnWheel(page, notches);
      await settled(page);
      expect((await stats(page)).visible.firstRow).toBe(500_000);
      expect((await click(page, await frozenPoint(page)))?.row).toBe(0);
    }
    const corner = await at(page, 8, 8);
    expect(await click(page, corner)).toMatchObject({ row: 0, col: 0 });
  });

  /** A point of the frozen row. */
  async function frozenPoint(page: Page) {
    const { metrics } = await stats(page);
    return at(page, metrics.colWidth * 2.5, metrics.rowHeight / 2);
  }

  async function scrollToRowUnderHeader(page: Page, row: number): Promise<void> {
    const { metrics } = await stats(page);
    await page.evaluate(
      (top) => window.__reticula?.scrollTo({ top }),
      (row - 1) * metrics.rowHeight,
    );
    await page.waitForFunction((row) => window.__reticula?.stats().visible.firstRow === row, row);
    await settled(page);
  }

  test("every sheet has a selection, and is found the way it was left", async ({ page }) => {
    // Nothing has been clicked yet, and nothing has the focus that it did not have.
    expect((await stats(page)).selection).toMatchObject({ name: "A1" });
    expect(await page.evaluate(() => document.activeElement === document.body)).toBe(true);

    await chooseZoom(page, 75);
    await click(page, await middleOf(page, 6, 3));
    const left = await stats(page);
    await page.getByRole("button", { name: "Add worksheet" }).click();
    await page.waitForFunction(
      (rows) => window.__reticula?.stats().sheet.rows !== rows,
      left.sheet.rows,
    );
    await zoomIs(page, 1);
    expect((await stats(page)).selection).toMatchObject({ name: "A1" });
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press("ArrowRight");
    expect((await stats(page)).selection).toMatchObject({ name: "B2" });

    await page.locator(".sheet-tab").first().click();
    await zoomIs(page, 0.75);
    expect((await stats(page)).selection).toMatchObject({ name: "D7" });
    await page.keyboard.press("ArrowUp");
    expect((await stats(page)).selection).toMatchObject({ name: "D6" });
    await page.locator(".sheet-tab").last().click();
    await zoomIs(page, 1);
    expect((await stats(page)).selection).toMatchObject({ name: "B2" });
  });

  test("the end of a sheet is found again, whatever comes to be around the grid", async ({
    page,
  }) => {
    await page.evaluate(() => {
      const hooks = window.__reticula;
      hooks?.scrollTo({ top: hooks.stats().scrollLimits.top });
    });
    await settled(page);
    const { visible, sheet } = await stats(page);
    expect(visible.endRow).toBe(sheet.rows);
    await page.reload();
    await ready(page);
    await expect.poll(async () => (await stats(page)).visible).toEqual(visible);
  });

  test("the menu of the steps is within a low window", async ({ page }) => {
    const { width } = page.viewportSize() ?? { width: 1280 };
    await page.setViewportSize({ width, height: 320 });
    await page.getByTestId("zoom-value").click();
    const menu = await page.getByTestId("zoom-menu").boundingBox();
    expect(menu?.y).toBeGreaterThanOrEqual(0);
    await page.getByTestId("zoom-25").click();
    await zoomIs(page, 0.25);
  });

  test("a draft is looked at from where the document was left, and leaves that as it was", async ({
    page,
  }) => {
    await chooseZoom(page, 75);
    await click(page, await middleOf(page, 6, 3));
    const editor = page.locator(".reticula-editor [contenteditable]");
    await page.getByTestId("ai-panel-btn").click();
    await page.getByTestId("ai-demo-regional-sales-summary").click();
    await expect(page.getByTestId("ai-draft-banner")).toBeVisible();
    await zoomIs(page, 0.75);
    expect((await stats(page)).selection).not.toBeNull();
    await editor.focus();
    await page.keyboard.press("ControlOrMeta+Home");
    await page.keyboard.press("ArrowDown");
    expect((await stats(page)).selection).toMatchObject({ name: "A2" });
    await chooseZoom(page, 50);
    const stored = () => page.evaluate(() => localStorage.getItem("reticula:view:v1") ?? "");
    const written = await stored();
    // Longer than it takes for a change of the view to be written down.
    await page.waitForTimeout(700);
    expect(await stored()).toBe(written);

    await page.getByRole("button", { name: "Back to Main" }).click();
    await expect(page.getByTestId("ai-draft-banner")).toBeHidden();
    await zoomIs(page, 0.75);
    expect((await stats(page)).selection).toMatchObject({ name: "D7" });
    await editor.focus();
    await page.keyboard.press("ArrowDown");
    expect((await stats(page)).selection).toMatchObject({ name: "D8" });
  });

  test("what is typed is seen at a quarter of the size", async ({ page }) => {
    await chooseZoom(page, 25);
    await click(page, await middleOf(page, 40, 10));
    await page.keyboard.type("12");
    const editor = page.locator(".reticula-editor");
    await expect(editor).toHaveCSS("opacity", "1");
    const box = await editor.boundingBox();
    const cell = await middleOf(page, 40, 10);
    expect(box?.height).toBeGreaterThanOrEqual(14);
    expect(await editor.evaluate((element) => getComputedStyle(element).fontSize)).toBe("11px");
    expect(cell.x).toBeGreaterThan(box?.x ?? Number.POSITIVE_INFINITY);
    expect(cell.x).toBeLessThan((box?.x ?? 0) + (box?.width ?? 0));
    await page.keyboard.press("Enter");
    await page.waitForFunction(() => window.__reticula?.textAt(40, 10)?.includes("12"));
  });

  test("text with no room on its right ends with the cells and is followed as it is typed", async ({
    page,
  }) => {
    for (const percent of [100, 200]) {
      await chooseZoom(page, percent);
      await scrollToRow(page, 100);
      const { metrics, visible } = await stats(page);
      await page.evaluate(([row, col]) => window.__reticula?.select(row, col), [
        104,
        visible.endCol - 2,
      ] as const);
      await page.locator(".reticula-editor [contenteditable]").focus();
      await page.keyboard.type("a text that is longer than there is room for. ".repeat(3));
      const grid = await gridBox(page);
      const editor = await page.locator(".reticula-editor").boundingBox();
      const right = (editor?.x ?? 0) + (editor?.width ?? 0);
      const end = grid.x + grid.width - metrics.scrollbarSize;
      expect(Math.abs(right - end)).toBeLessThanOrEqual(0.5);
      const scrolled = await page
        .locator(".reticula-editor .cm-scroller")
        .evaluate((element) => element.scrollLeft);
      expect(scrolled).toBeGreaterThan(0);
      await page.keyboard.press("Escape");
    }
  });

  test("a damaged memory of the last visit is no hindrance", async ({ page }) => {
    const far = { row: 9e9, rowFraction: 0.5, col: 9e9, colFraction: 0, selection: null };
    for (const [text, zoom] of [
      ["{", 1],
      ['{"documents":[{"id":"sample-1m","sheets":{"0":{"zoom":9}}}]}', 1],
      [
        JSON.stringify({ documents: [{ id: "sample-1m", sheets: { 0: { ...far, zoom: 0.5 } } }] }),
        0.5,
      ],
    ] as const) {
      // Before the page reads it, and after the one before has written its own.
      await page.addInitScript((text) => localStorage.setItem("reticula:view:v1", text), text);
      await page.goto("/app?hooks");
      await ready(page);
      const now = await stats(page);
      expect(now.zoom).toBe(zoom);
      const last = zoom === 1 ? 0 : now.sheet.rows - 1;
      expect(now.selection).toMatchObject({ row: last, scope: "cells" });
      expect(now.visible.endRow).toBeGreaterThan(last - 100);
    }
  });
});

test.describe("with a server", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "the rules are the same everywhere");

  let server: CollabServerHandle | null = null;

  test.beforeAll(async () => {
    server = await startCollabServer();
  });

  test.afterAll(async () => {
    await server?.stop();
    server = null;
  });

  test("the history is stepped through with the place, the zoom and a working keyboard kept", async ({
    page,
  }) => {
    if (!server) {
      throw new Error("the server has not been started");
    }
    await signInDev(page, server.baseUrl, "alice");
    await page.getByTestId("new-cloud-title-input").fill("Zoom and history");
    await page.getByTestId("new-cloud-sheet-btn").click();
    const pill = page.getByTestId("collab-status-pill");
    await expect(pill).toHaveText("Synced", { timeout: 15_000 });
    await settled(page);
    await editCell(page, 0, 0, "A1", "100");
    await editCell(page, 0, 0, "A1", "250");
    await expect(pill).toHaveText("Synced", { timeout: 10_000 });
    await chooseZoom(page, 50);
    await click(page, await middleOf(page, 6, 3));

    await page.getByTestId("history-btn").click();
    const back = page.getByRole("button", { name: "Previous revision" });
    const editor = page.locator(".reticula-editor [contenteditable]");
    await back.click();
    await expect(page.getByTestId("history-preview-banner")).toBeVisible({ timeout: 10_000 });
    await zoomIs(page, 0.5);
    expect((await stats(page)).selection).toMatchObject({ name: "D7" });
    await page.waitForFunction(() => window.__reticula?.textAt(0, 0) === "100");
    await editor.focus();
    await page.keyboard.press("ArrowDown");
    expect((await stats(page)).selection).toMatchObject({ name: "D8" });
    await chooseZoom(page, 75);
    await back.click();
    await page.waitForFunction(() => window.__reticula?.textAt(0, 0) === "");
    // The step is taken from where the one before was left.
    await zoomIs(page, 0.75);
    expect((await stats(page)).selection).toMatchObject({ name: "D8" });

    await page.getByTestId("history-preview-banner").getByRole("button").click();
    await expect(page.getByTestId("history-preview-banner")).toHaveCount(0);
    await zoomIs(page, 0.5);
    expect((await stats(page)).selection).toMatchObject({ name: "D7" });
  });
});
