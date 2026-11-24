import { type CDPSession, expect, type Page, test } from "@playwright/test";
import {
  type CollabServerHandle,
  createAndShareSheet,
  editCell,
  joinViaSharePath,
  signInDev,
  startCollabServer,
} from "./collab-helpers.ts";

// Fingers on the grid, and what the editor is on a device with nothing else.
// Pans and pinches are Chromium's touch events; the other browsers can tap.
// None of this has been tried on a real phone or tablet.

interface Point {
  readonly x: number;
  readonly y: number;
}

const PHONE = { width: 390, height: 844 };

function stats(page: Page) {
  return page.evaluate(() => {
    const hooks = window.__reticula;
    if (!hooks) {
      throw new Error("the test hooks are not installed");
    }
    return hooks.stats();
  });
}

function nextFrames(page: Page): Promise<void> {
  return page.evaluate(
    () =>
      new Promise<void>((done) => requestAnimationFrame(() => requestAnimationFrame(() => done()))),
  );
}

async function settled(page: Page): Promise<void> {
  await nextFrames(page);
  await page.waitForFunction(() => {
    const stats = window.__reticula?.stats();
    return stats !== undefined && stats.cellsDrawn > 0 && stats.missingCells === 0;
  });
}

async function ready(page: Page): Promise<void> {
  await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
  await settled(page);
}

/** What the workbook is, and how many operations have been made on it. */
function content(page: Page) {
  return page.evaluate(async () => ({
    digest: await window.__reticula?.stateDigest(),
    operations: window.__reticula?.docStatus().walSeq,
  }));
}

/** A point of the page by its distance from the corner of the first cell. */
async function at(page: Page, x: number, y: number): Promise<Point> {
  const box = await page.getByTestId("grid").boundingBox();
  if (!box) {
    throw new Error("the grid has no box");
  }
  const { metrics } = await stats(page);
  return { x: box.x + metrics.headerWidth + x, y: box.y + metrics.headerHeight + y };
}

/** Where the middle of a cell in view is, when no column or row has a size of its own. */
async function middleOf(page: Page, row: number, col: number): Promise<Point> {
  const { metrics, scroll } = await stats(page);
  return at(
    page,
    (col + 0.5) * metrics.colWidth - scroll.left,
    (row + 0.5) * metrics.rowHeight - scroll.top,
  );
}

/** What is at a point of the page, in rows and columns from the first one. */
async function sheetPointAt(page: Page, point: Point) {
  const { metrics, scroll } = await stats(page);
  const corner = await at(page, 0, 0);
  return {
    row: (point.y - corner.y + scroll.top) / metrics.rowHeight,
    col: (point.x - corner.x + scroll.left) / metrics.colWidth,
  };
}

async function scrollToRow(page: Page, row: number): Promise<void> {
  const { metrics } = await stats(page);
  await page.evaluate((top) => window.__reticula?.scrollTo({ top }), row * metrics.rowHeight);
  await settled(page);
}

const editor = (page: Page) => page.locator(".reticula-editor [contenteditable]");

/** That nobody has begun to edit anything, and no keyboard would be shown. */
async function isLookedAtOnly(page: Page): Promise<void> {
  await expect(page.locator(".reticula-editor[data-editing]")).toHaveCount(0);
  await expect(editor(page)).toHaveAttribute("inputmode", "none");
  await expect(editor(page)).toHaveAttribute("aria-readonly", "true");
}

/**
 * Fingers on the screen. Where it matters how soon one thing follows another,
 * the events say so themselves: the machine may be busy.
 */
async function fingers(page: Page) {
  const session: CDPSession = await page.context().newCDPSession(page);
  let clock = 0;
  const send = (
    type: "touchStart" | "touchMove" | "touchEnd" | "touchCancel",
    touchPoints: (Point & { readonly id: number })[],
    afterMs?: number,
  ) => {
    clock = afterMs === undefined ? Math.max(clock + 1, Date.now()) : clock + afterMs;
    return session.send("Input.dispatchTouchEvent", {
      type,
      touchPoints,
      timestamp: clock / 1000,
    });
  };
  const numbered = (points: readonly Point[]) => points.map(({ x, y }, id) => ({ x, y, id }));
  const tap = async (point: Point, afterMs?: number) => {
    await send("touchStart", numbered([point]), afterMs);
    await send("touchEnd", [], 30);
  };
  return {
    down: (...points: Point[]) => send("touchStart", numbered(points)),
    move: (...points: Point[]) => send("touchMove", numbered(points)),
    /** Lifts one of several fingers, the first of which is number 0. */
    lift: (id: number, { x, y }: Point) => send("touchEnd", [{ x, y, id }]),
    up: () => send("touchEnd", []),
    cancel: () => send("touchCancel", []),
    tap: (point: Point) => tap(point),
    async doubleTap(point: Point) {
      await tap(point);
      await tap(point, 90);
    },
    /** One finger, from one point to another, which it leaves when it has rested there. */
    async drag(from: Point, to: Point, steps = 5) {
      await send("touchStart", numbered([from]));
      for (let step = 1; step <= steps; step++) {
        const part = step / steps;
        const x = from.x + (to.x - from.x) * part;
        const y = from.y + (to.y - from.y) * part;
        await send("touchMove", numbered([{ x, y }]), 16);
      }
      await send("touchEnd", [], 200);
      await nextFrames(page);
    },
    /** One finger that lets the sheet go while it still moves. */
    async swipe(from: Point, by: number) {
      await send("touchStart", numbered([from]));
      for (let step = 1; step <= 6; step++) {
        await send("touchMove", numbered([{ x: from.x, y: from.y + (by * step) / 6 }]), 16);
      }
      await send("touchEnd", [], 8);
    },
  };
}

async function atRest(page: Page): Promise<number> {
  let last = -1;
  await expect
    .poll(
      async () => {
        const { top } = (await stats(page)).scroll;
        const isSame = top === last;
        last = top;
        return isSame;
      },
      { intervals: [120], timeout: 10_000 },
    )
    .toBe(true);
  return last;
}

test.describe("fingers", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "needs the browser's touch events");
  test.use({ viewport: PHONE, hasTouch: true });

  test.beforeEach(async ({ page }) => {
    await page.goto("/app?hooks");
    await ready(page);
  });

  test("scroll both ways as far as they move, and do nothing else", async ({ page }) => {
    const before = await content(page);
    const selected = (await stats(page)).selection?.name;
    const touch = await fingers(page);
    const from = await at(page, 250, 400);
    await touch.down(from);
    await touch.move({ x: from.x - 20, y: from.y - 20 });
    await nextFrames(page);
    const start = (await stats(page)).scroll;
    await touch.move({ x: from.x - 90, y: from.y - 160 });
    await nextFrames(page);
    const after = (await stats(page)).scroll;
    expect(after.left - start.left).toBeCloseTo(70, 3);
    expect(after.top - start.top).toBeCloseTo(140, 3);
    await page.waitForTimeout(150);
    await touch.up();
    await page.waitForTimeout(200);
    expect((await stats(page)).scroll).toEqual(after);
    expect((await stats(page)).selection?.name).toBe(selected);

    // Along an axis, the sheet does not sway.
    await touch.drag(await at(page, 250, 100), await at(page, 238, 400));
    const back = (await stats(page)).scroll;
    expect(back.top).toBe(0);
    expect(back.left).toBeCloseTo(after.left, 3);
    // The beginning of the sheet is as far as it goes.
    await touch.drag(await at(page, 20, 100), await at(page, 300, 110));
    expect((await stats(page)).scroll).toEqual({ top: 0, left: 0 });

    // What a mouse would fill, resize or move is scrolled with as well.
    const { metrics } = await stats(page);
    await touch.tap(await middleOf(page, 1, 1));
    await touch.drag(
      await at(page, metrics.colWidth * 2, metrics.rowHeight * 2),
      await at(page, metrics.colWidth * 2, metrics.rowHeight * 8),
    );
    await touch.drag(
      await at(page, metrics.colWidth, -10),
      await at(page, metrics.colWidth + 60, -10),
    );
    await touch.tap(await at(page, metrics.colWidth / 2, -10));
    await touch.drag(await at(page, metrics.colWidth / 2, -10), await at(page, 250, -10));
    expect((await stats(page)).selection?.name).toBe("A1:A1000000");
    expect((await stats(page)).metrics).toEqual(metrics);
    expect(await content(page)).toEqual(before);
    await isLookedAtOnly(page);
  });

  test("let the sheet go, which comes to rest or is stopped", async ({ page }) => {
    const touch = await fingers(page);
    const selected = (await stats(page)).selection?.name;
    const from = await at(page, 120, 500);
    await touch.swipe(from, -240);
    const left = (await stats(page)).scroll.top;
    expect(left).toBeGreaterThan(150);
    await expect.poll(async () => (await stats(page)).scroll.top).toBeGreaterThan(left + 100);
    const end = await atRest(page);
    expect(end).toBeGreaterThan(left + 100);
    expect((await stats(page)).scroll.left).toBe(0);

    // A finger stops it, and does nothing else.
    await touch.swipe(from, -240);
    await page.waitForTimeout(60);
    await touch.tap(await at(page, 100, 300));
    const stopped = (await stats(page)).scroll.top;
    await page.waitForTimeout(200);
    expect((await stats(page)).scroll.top).toBe(stopped);
    expect((await stats(page)).selection?.name).toBe(selected);

    // So does what else moves the sheet.
    for (const stop of ["wheel", "zoom", "key", "mouse"] as const) {
      await touch.swipe(from, -240);
      await page.waitForTimeout(40);
      if (stop === "wheel") {
        await page.mouse.move(from.x, from.y);
        await page.mouse.wheel(0, 10);
      } else if (stop === "zoom") {
        await page.getByTestId("zoom-in").evaluate((button: HTMLElement) => button.click());
      } else if (stop === "key") {
        await editor(page).evaluate((element) =>
          element.dispatchEvent(new KeyboardEvent("keydown", { key: "Shift", bubbles: true })),
        );
      } else {
        await page.mouse.click(5, 5);
      }
      await nextFrames(page);
      await nextFrames(page);
      const { top } = (await stats(page)).scroll;
      await page.waitForTimeout(150);
      expect((await stats(page)).scroll.top, stop).toBe(top);
    }

    // The end of the sheet is the end.
    await page.evaluate(() => {
      const hooks = window.__reticula;
      hooks?.scrollTo({ top: hooks.stats().scrollLimits.top - 50 });
    });
    await touch.swipe(from, -240);
    expect(await atRest(page)).toBe((await stats(page)).scrollLimits.top);
  });

  test("do not let the sheet go where motion is to be reduced", async ({ page }) => {
    await page.emulateMedia({ reducedMotion: "reduce" });
    const touch = await fingers(page);
    await touch.swipe(await at(page, 120, 500), -240);
    await nextFrames(page);
    const { top } = (await stats(page)).scroll;
    expect(top).toBeGreaterThan(150);
    await page.waitForTimeout(200);
    expect((await stats(page)).scroll.top).toBe(top);
  });

  test("pinch by steps of five per cent, with what is between them staying there", async ({
    page,
  }) => {
    await scrollToRow(page, 500_000);
    const before = await content(page);
    const touch = await fingers(page);
    const middle = await at(page, 150, 300);
    const apart = (span: number): [Point, Point] => [
      { x: middle.x, y: middle.y - span / 2 },
      { x: middle.x, y: middle.y + span / 2 },
    ];
    const pinch = async (from: number, to: number) => {
      const steps = 12;
      await touch.down(...apart(from));
      for (let step = 1; step <= steps; step++) {
        await touch.move(...apart(from + ((to - from) * step) / steps));
        await nextFrames(page);
        const { zoom } = await stats(page);
        expect(zoom * 20).toBeCloseTo(Math.round(zoom * 20), 6);
      }
    };
    const start = await sheetPointAt(page, middle);
    await pinch(100, 168);
    await nextFrames(page);
    const { zoom } = await stats(page);
    expect(zoom).toBeGreaterThan(1.4);
    expect(zoom).toBeLessThanOrEqual(1.5);
    const larger = await sheetPointAt(page, middle);
    expect(larger.row).toBeCloseTo(start.row, 1);

    // One finger is lifted; the other goes on with the sheet, from where it is.
    const [stays, lifted] = apart(168);
    await touch.lift(1, lifted);
    await nextFrames(page);
    const { top } = (await stats(page)).scroll;
    await touch.move({ x: stays.x, y: stays.y + 50 });
    await nextFrames(page);
    expect((await stats(page)).scroll.top).toBeCloseTo(top - 50, 1);
    await page.waitForTimeout(150);
    await touch.up();

    const other = await sheetPointAt(page, middle);
    await pinch(250, 80);
    await touch.up();
    await nextFrames(page);
    await settled(page);
    expect((await stats(page)).zoom).toBeLessThan(0.6);
    expect((await sheetPointAt(page, middle)).row).toBeCloseTo(other.row, 1);

    // All the way, and no further.
    await pinch(300, 20);
    await touch.up();
    await nextFrames(page);
    expect((await stats(page)).zoom).toBe(0.25);
    await expect(page.getByTestId("zoom-value")).toHaveText("25%");
    expect(await content(page)).toEqual(before);
    expect((await stats(page)).selection?.name).toBe("A1");
    await isLookedAtOnly(page);
  });

  test("move the sheet with both of them, without a pinch", async ({ page }) => {
    await scrollToRow(page, 500_000);
    const touch = await fingers(page);
    const one = await at(page, 80, 300);
    const other = await at(page, 180, 300);
    const start = await sheetPointAt(page, one);
    await touch.down(one, other);
    for (let step = 1; step <= 8; step++) {
      await touch.move({ x: one.x, y: one.y - step * 12 }, { x: other.x, y: other.y - step * 12 });
    }
    await nextFrames(page);
    const end = await sheetPointAt(page, { x: one.x, y: one.y - 96 });
    expect(end.row).toBeCloseTo(start.row, 1);
    await touch.up();
    expect((await stats(page)).zoom).toBe(1);
  });

  test("that are taken away end what they did, and are no hindrance to the next", async ({
    page,
  }) => {
    const touch = await fingers(page);
    const from = await at(page, 150, 500);
    await touch.down(from);
    await touch.move({ x: from.x, y: from.y - 100 });
    await touch.move({ x: from.x, y: from.y - 200 });
    await nextFrames(page);
    const { top } = (await stats(page)).scroll;
    expect(top).toBeGreaterThan(150);
    await touch.cancel();
    await page.waitForTimeout(150);
    expect((await stats(page)).scroll.top).toBe(top);

    const one = await at(page, 100, 200);
    const other = await at(page, 100, 300);
    await touch.down(one, other);
    await touch.move(one, { x: other.x, y: other.y + 80 });
    await nextFrames(page);
    const { zoom } = await stats(page);
    expect(zoom).toBeGreaterThan(1);
    await touch.cancel();
    await nextFrames(page);
    expect((await stats(page)).zoom).toBe(zoom);

    const { metrics, scroll } = await stats(page);
    const row = Math.floor(scroll.top / metrics.rowHeight) + 3;
    await touch.tap(await middleOf(page, row, 1));
    await expect.poll(async () => (await stats(page)).selection).toMatchObject({ row, col: 1 });
  });

  test("tap a cell, a header and the corner to select them", async ({ page }) => {
    const before = await content(page);
    // Mouse events in the wake of a tap would have the browser show a keyboard.
    await page.evaluate(() => {
      for (const type of ["mousedown", "mouseup"]) {
        window.addEventListener(type, () => document.body.setAttribute("data-mouse", ""), true);
      }
    });
    const touch = await fingers(page);
    const { metrics } = await stats(page);
    const tapped = async (point: Point) => {
      // Two taps in short order are something else.
      await page.waitForTimeout(350);
      await touch.tap(point);
      await nextFrames(page);
      return (await stats(page)).selection?.name;
    };
    expect(await tapped(await middleOf(page, 4, 1))).toBe("B5");
    await expect(page.getByTestId("name-box")).toHaveText("B5");
    await expect(editor(page)).toBeFocused();
    expect(await tapped(await at(page, metrics.colWidth * 1.5, -8))).toBe("B1:B1000000");
    // Where a mouse would resize the one before.
    expect(await tapped(await at(page, metrics.colWidth + 2, -8))).toBe("B1:B1000000");
    expect(await tapped(await at(page, -20, metrics.rowHeight * 6.5))).toBe("A7:V7");
    expect(await tapped(await at(page, -20, -8))).toBe("A1:V1000000");
    expect(await tapped(await middleOf(page, 2, 0))).toBe("A3");

    // A press that takes long is none.
    await page.waitForTimeout(350);
    const other = await middleOf(page, 8, 1);
    await touch.down(other);
    await page.waitForTimeout(800);
    await touch.up();
    await nextFrames(page);
    expect((await stats(page)).selection?.name).toBe("A3");
    await expect(page.getByTestId("context-menu")).toHaveCount(0);
    await expect(page.locator("body")).not.toHaveAttribute("data-mouse");
    expect(await content(page)).toEqual(before);
    await isLookedAtOnly(page);
  });

  test("tap twice to change between 100% and another zoom", async ({ page }) => {
    await scrollToRow(page, 500_000);
    const before = await content(page);
    const touch = await fingers(page);
    const point = await at(page, 150, 260);
    const start = await sheetPointAt(page, point);
    const twice = async (zoom: number) => {
      await page.waitForTimeout(400);
      await touch.doubleTap(point);
      await page.waitForFunction((zoom) => window.__reticula?.stats().zoom === zoom, zoom);
      expect((await sheetPointAt(page, point)).row).toBeCloseTo(start.row, 1);
    };
    await twice(1.5);
    await twice(1);
    await page.getByTestId("zoom-value").tap();
    await page.getByTestId("zoom-75").tap();
    await page.waitForFunction(() => window.__reticula?.stats().zoom === 0.75);
    await page.evaluate((top) => window.__reticula?.scrollTo({ top }), 500_000 * 18);
    await settled(page);
    Object.assign(start, await sheetPointAt(page, point));
    await twice(1);
    await twice(0.75);
    expect(await content(page)).toEqual(before);
    await isLookedAtOnly(page);
  });

  test("leave a filter alone, and the keyboard looks around", async ({ page }) => {
    await page.keyboard.press("Control+/");
    await page.getByTestId("palette-input").fill("autofilter");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("command-palette")).toHaveCount(0);
    await settled(page);
    const before = await content(page);
    const touch = await fingers(page);
    const { metrics } = await stats(page);
    const selected = (await stats(page)).selection?.name;
    await touch.tap(await at(page, metrics.colWidth * 2 - 12, metrics.rowHeight / 2));
    await page.waitForTimeout(100);
    await expect(page.getByTestId("filter-modal")).toHaveCount(0);
    await expect(page.locator("dialog[open]")).toHaveCount(0);
    expect((await stats(page)).selection?.name).toBe(selected);

    await page.waitForTimeout(350);
    await touch.tap(await middleOf(page, 5, 1));
    await expect(page.getByTestId("name-box")).toHaveText("B6");
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press("Shift+ArrowRight");
    await expect(page.getByTestId("name-box")).toHaveText("B7:C7");
    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.keyboard.press("Control+c");
    await expect
      .poll(() => page.evaluate(() => navigator.clipboard.readText()))
      .toBe(
        await page.evaluate(() =>
          [1, 2].map((col) => window.__reticula?.textAt(6, col)).join("\t"),
        ),
      );
    await page.keyboard.type("12");
    await page.keyboard.press("Enter");
    await page.keyboard.press("Delete");
    await page.keyboard.press("F2");
    await page.keyboard.press("Control+b");
    await page.keyboard.press("Control+d");
    await nextFrames(page);
    expect(await content(page)).toEqual(before);
    await isLookedAtOnly(page);
  });
});

test.describe("a touch screen and nothing else", () => {
  test.use({ viewport: PHONE, hasTouch: true });

  test("has the editor for looking only", async ({ page, browserName }) => {
    await page.goto("/app?hooks");
    await ready(page);
    const before = await content(page);
    await expect(page.locator(".formatbar")).toBeHidden();
    await expect(page.getByTestId("undo-btn")).toBeHidden();
    await expect(page.getByTestId("insert-row-above")).toBeHidden();
    await expect(page.getByTestId("add-sheet-btn")).toBeHidden();
    await expect(page.getByTestId("formula-input")).toHaveCount(0);
    await isLookedAtOnly(page);
    expect(await page.evaluate(() => document.activeElement === document.body)).toBe(true);

    const tap = async (point: Point) => page.touchscreen.tap(point.x, point.y);
    await tap(await middleOf(page, 1, 1));
    await expect(page.getByTestId("name-box")).toHaveText("B2");
    const bar = page.getByTestId("touch-bar");
    await expect(bar.getByTestId("touch-value")).toHaveText(
      (await page.evaluate(() => window.__reticula?.textAt(1, 1))) ?? "",
    );
    await expect(bar.getByTestId("touch-formula")).toHaveCount(0);
    await page.evaluate(() => {
      window.__reticula?.scrollTo({ left: Number.MAX_SAFE_INTEGER });
      window.__reticula?.select(1, 21);
    });
    await settled(page);
    await expect(bar.getByTestId("touch-formula")).toHaveText("=SUM(M2:M100001)");
    await expect(bar.getByTestId("touch-value")).toHaveText(/^\d+\.\d\d$/);
    await expect(bar.locator(".touchbar-text")).toHaveCSS("user-select", "text");

    await editor(page).focus();
    await page.keyboard.type("5");
    await page.keyboard.press("Enter");
    await page.keyboard.press("Delete");
    await nextFrames(page);
    expect(await content(page)).toEqual(before);
    await isLookedAtOnly(page);

    await page.getByTestId("name-box").tap();
    await page.locator("input.name-box").fill("D9");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("name-box")).toHaveText("D9");
    expect((await stats(page)).selection).toMatchObject({ row: 8, col: 3 });

    for (const target of ["zoom-in", "zoom-value", "zoom-out", "sheet-tab-0"]) {
      const box = await page.getByTestId(target).boundingBox();
      expect(box?.width, target).toBeGreaterThanOrEqual(44);
      expect(box?.height, target).toBeGreaterThanOrEqual(44);
    }
    await page.getByTestId("zoom-in").tap();
    await expect(page.getByTestId("zoom-value")).toHaveText("110%");
    await page.getByTestId("zoom-value").tap();
    await page.getByTestId("zoom-50").tap();
    await expect(page.getByTestId("zoom-value")).toHaveText("50%");

    // Every command that would change anything is off.
    if (browserName === "chromium") {
      await page.getByTestId("view-menu-btn").tap();
      await page.getByTestId("menu-palette").tap();
      for (const id of [
        "insertRowAbove",
        "bold",
        "paste",
        "moveRowsDown",
        "colWidth",
        "addSheet",
        "freeze:row1",
        "autofitCols",
      ]) {
        await page.getByTestId("palette-input").fill("");
        await expect(page.getByTestId(`command-${id}`)).toHaveAttribute("aria-disabled", "true");
      }
      await page.keyboard.press("Escape");
    }
  });

  test("has what fingers do before there are any", async ({ page }) => {
    await page.goto("/app?hooks");
    await ready(page);
    await page.waitForFunction(() =>
      performance.getEntriesByType("resource").some(({ name }) => name.includes("/touch-input-")),
    );
  });

  test("has a card in place of the editor first, without the engine", async ({ page }) => {
    const requests: string[] = [];
    page.on("request", (request) => requests.push(new URL(request.url()).pathname));
    await page.goto("/");
    await page.getByTestId("open-demo").tap();
    await expect(page.getByTestId("mobile-fallback")).toBeVisible();
    await page.goto("/app");
    await expect(page.getByTestId("mobile-fallback")).toBeVisible();
    await expect(page.getByTestId("open-anyway")).toHaveCount(0);
    await page.waitForLoadState("networkidle");
    expect(requests.filter((path) => /\.wasm$|\/worker-/.test(path))).toEqual([]);

    const open = page.getByTestId("open-read-only");
    await expect(open).toHaveText("Open read-only");
    await expect(open).toHaveAccessibleDescription(/Connect a mouse or a trackpad/);
    await open.tap();
    await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
    await expect(page.getByTestId("touch-bar")).toBeVisible();
    await expect(editor(page)).toHaveAttribute("inputmode", "none");

    // A phone that is turned keeps what it shows.
    await page.setViewportSize({ width: PHONE.height, height: PHONE.width });
    await page.goto("/app");
    await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
    await expect(page.getByTestId("touch-bar")).toBeVisible();
    await page.setViewportSize(PHONE);
    await page.waitForTimeout(200);
    await expect(page.getByTestId("touch-bar")).toBeVisible();
    await expect(page.getByTestId("mobile-fallback")).toHaveCount(0);
  });

  test("has recordings reviewed, and merges nothing of them", async ({ page, browserName }) => {
    test.skip(browserName !== "chromium", "the rules are the same everywhere");
    await page.goto("/app?hooks");
    await ready(page);
    const before = await content(page);
    await page.getByTestId("ai-panel-btn").tap();
    await page.getByTestId("ai-demo-regional-sales-summary").tap();
    await expect(page.getByTestId("ai-draft-banner")).toBeVisible();
    await page.getByTestId("ai-replay-review-btn").tap();
    await expect(page.getByTestId("ai-diff-review")).toBeVisible();
    const body = page.locator(".ai-panel-body");
    expect(await body.evaluate((one) => one.scrollWidth <= one.clientWidth)).toBe(true);
    await page.getByTestId("ai-merge-apply").tap();
    await expect(page.getByTestId("ai-panel")).toContainText("Nothing was merged");
    await expect(page.getByTestId("ai-draft-banner")).toBeHidden();
    expect(await content(page)).toEqual(before);
  });

  test("has the card with the hooks too when it is told to", async ({ page }) => {
    await page.goto("/app?hooks&touch=1");
    await page.getByTestId("open-read-only").tap();
    await ready(page);
    await expect(page.getByTestId("touch-bar")).toBeVisible();
    await page.goto("/app?hooks&touch=0");
    await ready(page);
    await expect(page.getByTestId("formula-input")).toBeVisible();
    await expect(page.locator(".formatbar")).toBeVisible();
  });

  test("goes straight to the workbook a link leads to", async ({ page }) => {
    await page.route("**/api/**", (route) => route.fulfill({ status: 404, json: { error: "" } }));
    await page.goto("/join/some-token");
    await expect(page.getByTestId("grid")).toBeVisible();
    await expect(page.locator(".app")).toHaveAttribute("data-touch", "true");
    await expect(page.getByTestId("mobile-fallback")).toHaveCount(0);
  });
});

test("a narrow window with a mouse has the card too, also for a link", async ({ page }) => {
  await page.setViewportSize(PHONE);
  for (const path of ["/app", "/join/some-token"]) {
    await page.goto(path);
    await expect(page.getByTestId("mobile-fallback")).toContainText("open this link on desktop");
    await expect(page.getByTestId("open-anyway")).toHaveText("Open editor anyway");
    await expect(page.getByTestId("open-read-only")).toHaveCount(0);
  }
});

test.describe("the parts of the window", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "the rules are the same everywhere");
  test.use({ hasTouch: true });

  for (const [width, height] of [
    [360, 640],
    [390, 844],
    [430, 932],
    [667, 375],
    [844, 390],
  ] as const) {
    test(`are within ${width} × ${height}`, async ({ page }) => {
      await page.setViewportSize({ width, height });
      for (const lang of ["en", "zh-CN"]) {
        await page.goto(`/app?hooks&lang=${lang}`);
        await page.waitForFunction(() => (window.__reticula?.stats().cellsDrawn ?? 0) > 0);
        await expect(page.getByTestId("touch-bar")).toBeVisible();
        const isWithin = async (selector: string) => {
          const box = await page.locator(selector).boundingBox();
          expect(box?.x, selector).toBeGreaterThanOrEqual(0);
          expect(box?.y, selector).toBeGreaterThanOrEqual(0);
          expect((box?.x ?? 0) + (box?.width ?? 0), selector).toBeLessThanOrEqual(width);
          expect((box?.y ?? 0) + (box?.height ?? 0), selector).toBeLessThanOrEqual(height);
        };
        const doesNotOverflow = () =>
          page.evaluate(() => {
            const { scrollWidth, scrollHeight } = document.documentElement;
            return scrollWidth <= window.innerWidth && scrollHeight <= window.innerHeight;
          });
        for (const selector of [
          ".titlebar",
          ".touchbar",
          ".sheetbar",
          ".statusbar",
          ".zoom-control",
        ]) {
          await isWithin(selector);
        }
        expect(await doesNotOverflow()).toBe(true);
        const grid = await page.getByTestId("grid").boundingBox();
        expect(grid?.height).toBeGreaterThan(height * 0.45);
        // What does not fit in the title bar can be scrolled to.
        await page.getByTestId("view-menu-btn").tap();
        await expect(page.getByTestId("view-menu")).toBeInViewport({ ratio: 0.99 });
        await page.keyboard.press("Escape");

        await page.getByTestId("ai-panel-btn").tap();
        await expect(page.getByTestId("ai-panel")).toBeVisible();
        await isWithin(".ai-panel");
        expect(
          await page
            .locator(".ai-panel-body")
            .evaluate((body) => body.scrollWidth <= body.clientWidth),
        ).toBe(true);
        await page.getByTestId("ai-close-btn").tap();
        await expect(page.getByTestId("ai-panel")).toBeHidden();

        // A long text wraps, and does not take the place of the grid.
        await page.evaluate(() => {
          window.__reticula?.scrollTo({ left: 1_000 });
          window.__reticula?.select(1, 12);
        });
        await expect(page.getByTestId("touch-formula")).toContainText("ROUND");
        await isWithin(".touchbar");
        expect(await doesNotOverflow()).toBe(true);
      }
    });
  }
});

test.describe("what is connected", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "needs the browser's emulation");
  test.use({ hasTouch: true });

  test("decides on what the editor is for, as it comes and goes", async ({ page }) => {
    await page.goto("/app?hooks");
    await ready(page);
    await isLookedAtOnly(page);
    const session = await page.context().newCDPSession(page);
    await session.send("Emulation.setTouchEmulationEnabled", { enabled: false });
    await expect(page.getByTestId("formula-input")).toBeVisible();
    await expect(page.getByTestId("touch-bar")).toHaveCount(0);
    await expect(editor(page)).not.toHaveAttribute("inputmode", /./);
    await expect(editor(page)).not.toHaveAttribute("aria-readonly", /./);
    const before = await content(page);

    // What is being typed when the mouse is gone has been typed with it there.
    const cell = await middleOf(page, 3, 2);
    await page.mouse.click(cell.x, cell.y);
    await page.keyboard.type("1234");
    await session.send("Emulation.setTouchEmulationEnabled", { enabled: true });
    await expect(page.getByTestId("touch-bar")).toBeVisible();
    await page.waitForFunction(() => window.__reticula?.textAt(3, 2)?.includes("1234"));
    await isLookedAtOnly(page);
    expect((await content(page)).digest).not.toBe(before.digest);

    // Nor does a dialog that was opened with it change anything without it.
    await session.send("Emulation.setTouchEmulationEnabled", { enabled: false });
    await expect(page.getByTestId("formula-input")).toBeVisible();
    await page.evaluate(() => window.__reticula?.select(0, 0));
    await page.keyboard.press("Control+/");
    await page.getByTestId("palette-input").fill("row height");
    await page.keyboard.press("Enter");
    await page.getByTestId("size-input").fill("90");
    await session.send("Emulation.setTouchEmulationEnabled", { enabled: true });
    await expect(page.getByTestId("touch-bar")).toBeVisible();
    const typed = await content(page);
    await page.getByTestId("size-apply").click();
    await expect(page.getByTestId("size-dialog")).toHaveCount(0);
    const second = await middleOf(page, 1, 1);
    await page.touchscreen.tap(second.x, second.y);
    await expect(page.getByTestId("name-box")).toHaveText("B2");
    expect(await content(page)).toEqual(typed);
  });
});

test.describe("a mouse next to a touch screen", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "needs the browser's touch events");

  test.beforeEach(async ({ page }) => {
    await page.goto("/app?hooks&touch=0");
    await ready(page);
  });

  test("works as it always does, whatever the fingers have done before", async ({ page }) => {
    const touch = await fingers(page);
    const { metrics } = await stats(page);
    await touch.drag(await at(page, 300, 300), await at(page, 300, 250));
    expect((await stats(page)).scroll.top).toBeGreaterThan(30);
    await page.evaluate(() => window.__reticula?.scrollTo({ top: 0, left: 0 }));
    await touch.tap(await middleOf(page, 2, 2));
    await expect(page.getByTestId("name-box")).toHaveText("C3");
    await expect(editor(page)).toBeFocused();
    await expect(editor(page)).toHaveAttribute("inputmode", "none");

    // Safari reports what two fingers do on a screen and on a trackpad in the same way.
    const gesture = () =>
      page.evaluate(() => {
        const input = document.querySelector(".reticula-input");
        return ["gesturestart", "gesturechange", "gestureend"].map((type) => {
          const place = { scale: 1.5, clientX: 300, clientY: 300 };
          const event = Object.assign(new Event(type, { bubbles: true, cancelable: true }), place);
          input?.dispatchEvent(event);
          return event.defaultPrevented;
        });
      });
    expect(await gesture()).toEqual([true, true, true]);
    await nextFrames(page);
    expect((await stats(page)).zoom).toBe(1);

    const from = await middleOf(page, 1, 1);
    const to = await middleOf(page, 4, 3);
    await page.mouse.move(from.x, from.y);
    expect(await gesture()).toEqual([true, true, true]);
    await nextFrames(page);
    expect((await stats(page)).zoom).toBe(1.5);
    await page.getByTestId("zoom-value").click();
    await page.getByTestId("zoom-100").click();
    await page.evaluate(() => window.__reticula?.scrollTo({ top: 0, left: 0 }));
    await nextFrames(page);
    await expect(editor(page)).toHaveAttribute("inputmode", "none");
    await page.mouse.move(from.x, from.y);
    await page.mouse.down();
    await expect(editor(page)).not.toHaveAttribute("inputmode", /./);
    await page.mouse.move(to.x, to.y, { steps: 4 });
    // A finger that comes to a drag is in the way.
    await touch.drag(await at(page, 500, 300), await at(page, 500, 200));
    expect((await stats(page)).scroll.top).toBe(0);
    await page.mouse.move(to.x + metrics.colWidth, to.y, { steps: 2 });
    await page.mouse.up();
    await expect(page.getByTestId("name-box")).toHaveText("B2:E5");

    // And a mouse that comes to a pan ends it.
    const start = await at(page, 300, 300);
    await touch.down(start);
    await touch.move({ x: start.x, y: start.y - 60 });
    await nextFrames(page);
    const { top } = (await stats(page)).scroll;
    expect(top).toBeGreaterThan(40);
    const cell = await middleOf(page, 9, 0);
    await page.mouse.click(cell.x, cell.y);
    await touch.move({ x: start.x, y: start.y - 160 });
    await touch.up();
    await nextFrames(page);
    expect((await stats(page)).scroll.top).toBe(top);
    await expect(page.getByTestId("name-box")).toHaveText("A10");

    await page.evaluate(() => window.__reticula?.scrollTo({ top: 0, left: 0 }));
    await nextFrames(page);
    const edited = await middleOf(page, 3, 3);
    await page.mouse.dblclick(edited.x, edited.y);
    await expect(page.locator(".reticula-editor[data-editing]")).toHaveCount(1);
    await page.keyboard.press("Escape");
    const menu = await middleOf(page, 5, 1);
    await page.mouse.click(menu.x, menu.y, { button: "right" });
    await expect(page.getByTestId("context-menu")).toBeVisible();
    await page.keyboard.press("Escape");
  });

  test("selects, moves and scrolls with the headers and past the edges", async ({ page }) => {
    const { metrics } = await stats(page);
    const { colWidth, rowHeight } = metrics;
    const name = async () => (await stats(page)).selection?.name;
    const column = (col: number) => at(page, (col + 0.5) * colWidth, -10);
    const row = (index: number) => at(page, -30, (index + 0.5) * rowHeight);
    const click = async ({ x, y }: Point, key?: "Shift" | "Control") => {
      if (key) {
        await page.keyboard.down(key);
      }
      await page.mouse.click(x, y);
      if (key) {
        await page.keyboard.up(key);
      }
    };
    const drag = async (from: Point, to: Point) => {
      await page.mouse.move(from.x, from.y);
      await page.mouse.down();
      await page.mouse.move(to.x, to.y, { steps: 4 });
    };

    await click(await column(1));
    expect(await name()).toBe("B1:B1000000");
    await click(await column(3), "Shift");
    expect(await name()).toBe("B1:D1000000");
    await click(await column(5), "Control");
    expect((await stats(page)).selection?.ranges).toHaveLength(2);
    await drag(await row(2), await row(5));
    await page.mouse.up();
    expect(await name()).toBe("A3:V6");
    // In the selection, the menu is of all of it.
    const inside = await row(4);
    await page.mouse.click(inside.x, inside.y, { button: "right" });
    await expect(page.getByTestId("context-menu")).toBeVisible();
    await page.keyboard.press("Escape");
    expect(await name()).toBe("A3:V6");
    const outside = await column(2);
    await page.mouse.click(outside.x, outside.y, { button: "right" });
    await expect(page.getByTestId("menu-colWidth")).toBeVisible();
    await page.keyboard.press("Escape");
    expect(await name()).toBe("C1:C1000000");

    // A line that is selected is moved, and pressed it is the only one.
    const texts = (count: number) =>
      page.evaluate(
        (count) =>
          Array.from({ length: count }, (_, col) => window.__reticula?.textAt(0, col) ?? ""),
        count,
      );
    const [a = "", b = "", c = "", d = ""] = await texts(4);
    await click(await column(0));
    await drag(await column(0), await at(page, colWidth * 2.9, -10));
    await page.mouse.up();
    await expect.poll(() => texts(4)).toEqual([b, c, a, d]);
    const first = await page.evaluate(() => [1, 2, 3].map((r) => window.__reticula?.textAt(r, 1)));
    await click(await row(1));
    await drag(await row(1), await at(page, -30, rowHeight * 3.9));
    await page.mouse.up();
    await expect
      .poll(() => page.evaluate(() => [1, 2, 3].map((r) => window.__reticula?.textAt(r, 1))))
      .toEqual([first[1], first[2], first[0]]);
    await drag(await row(1), await row(3));
    await page.mouse.up();
    await click(await row(2));
    expect(await name()).toBe("A3:V3");

    // Past the edge, the cells come to the pointer until it is back or lifted.
    const box = await page.getByTestId("grid").boundingBox();
    const below = { x: (box?.x ?? 0) + 300, y: (box?.y ?? 0) + (box?.height ?? 0) + 4 };
    await drag(await middleOf(page, 4, 2), below);
    await expect.poll(async () => (await stats(page)).scroll.top).toBeGreaterThan(rowHeight * 10);
    const back = await at(page, 250, 200);
    await page.mouse.move(back.x, back.y);
    const { top } = (await stats(page)).scroll;
    await page.waitForTimeout(150);
    expect((await stats(page)).scroll.top).toBe(top);
    await page.mouse.move(below.x, below.y);
    await page.mouse.up();
    const end = (await stats(page)).scroll.top;
    await page.waitForTimeout(150);
    expect((await stats(page)).scroll.top).toBe(end);
    expect((await stats(page)).selection?.range.endRow).toBeGreaterThan(20);

    // A fill goes the way the pointer has gone further.
    await page.evaluate(() => window.__reticula?.scrollTo({ top: 0, left: 0 }));
    await nextFrames(page);
    await click(await middleOf(page, 4, 4));
    await drag(
      await at(page, colWidth * 5, rowHeight * 5),
      await at(page, colWidth * 5.5, rowHeight * 7.5),
    );
    await page.mouse.up();
    await expect.poll(name).toBe("E5:E8");
    await click(await middleOf(page, 6, 6));
    await drag(
      await at(page, colWidth * 7, rowHeight * 7),
      await at(page, colWidth * 5.5, rowHeight * 6.5),
    );
    await page.mouse.up();
    await expect.poll(name).toBe("F7:G7");
  });

  test("leaves the keyboard to what the fingers have tapped", async ({ page }) => {
    const touch = await fingers(page);
    const before = await content(page);
    // Neither opens an editor or a menu.
    await touch.doubleTap(await middleOf(page, 2, 2));
    await page.waitForFunction(() => window.__reticula?.stats().zoom === 1.5);
    await page.waitForTimeout(350);
    const pressed = await at(page, 200, 200);
    await touch.down(pressed);
    await page.waitForTimeout(800);
    // A browser may take that for the wish for a menu.
    const isPrevented = await page.evaluate(({ x, y }) => {
      const event = new MouseEvent("contextmenu", {
        clientX: x,
        clientY: y,
        bubbles: true,
        cancelable: true,
      });
      document.querySelector(".reticula-input")?.dispatchEvent(event);
      return event.defaultPrevented;
    }, pressed);
    expect(isPrevented).toBe(true);
    await touch.up();
    await nextFrames(page);
    await expect(page.getByTestId("context-menu")).toHaveCount(0);
    expect((await stats(page)).selection).toMatchObject({ row: 2, col: 2 });
    await expect(page.locator(".reticula-editor[data-editing]")).toHaveCount(0);
    expect(await content(page)).toEqual(before);
    await page.getByTestId("zoom-out").click();
    await page.getByTestId("zoom-out").click();
    await page.getByTestId("zoom-out").click();
    await page.waitForFunction(() => window.__reticula?.stats().zoom === 1);

    await touch.tap(await middleOf(page, 2, 2));
    await expect(page.getByTestId("name-box")).toHaveText("C3");
    await expect(editor(page)).toHaveAttribute("inputmode", "none");
    await page.keyboard.type("77");
    await expect(editor(page)).not.toHaveAttribute("inputmode", /./);
    await expect(page.locator(".reticula-editor[data-editing]")).toHaveCount(1);
    // A pan leaves the editor open, a tap elsewhere accepts what is in it.
    await touch.drag(await at(page, 300, 300), await at(page, 300, 250));
    await expect(page.locator(".reticula-editor[data-editing]")).toHaveCount(1);
    await touch.tap(await middleOf(page, 6, 4));
    await expect(page.locator(".reticula-editor[data-editing]")).toHaveCount(0);
    await expect(page.getByTestId("name-box")).toHaveText("E7");
    await page.waitForFunction(() => window.__reticula?.textAt(2, 2)?.includes("77"));

    // Fingers pick no references: that would be editing.
    await page.keyboard.type("=");
    await expect(page.locator(".reticula-editor[data-editing]")).toHaveCount(1);
    await page.waitForTimeout(350);
    await touch.tap(await middleOf(page, 8, 1));
    await page.waitForTimeout(100);
    await expect(editor(page)).toHaveText("=");
    await expect(page.getByTestId("name-box")).toHaveText("E7");
    await page.keyboard.press("Escape");
    await page.waitForTimeout(350);
    await touch.tap(await middleOf(page, 8, 1));
    await expect(page.getByTestId("name-box")).toHaveText("B9");
  });

  test("a sheet that was let go does not go on in another one", async ({ page }) => {
    await page.getByTestId("add-sheet-btn").click();
    await page.waitForFunction(() => window.__reticula?.stats().sheet.cols !== 22);
    await nextFrames(page);
    const touch = await fingers(page);
    await touch.swipe(await at(page, 120, 400), -240);
    const { top } = (await stats(page)).scroll;
    expect(top).toBeGreaterThan(150);
    await page.getByTestId("sheet-tab-0").evaluate((tab: HTMLElement) => tab.click());
    await page.waitForFunction(() => window.__reticula?.stats().sheet.cols === 22);
    await page.waitForTimeout(200);
    expect((await stats(page)).scroll.top).toBe(0);
  });

  test("what fingers do is loaded with the first of them", async ({ page }) => {
    const isLoaded = () =>
      page.evaluate(() =>
        performance.getEntriesByType("resource").some(({ name }) => name.includes("/touch-input-")),
      );
    await page.waitForTimeout(500);
    expect(await isLoaded()).toBe(false);
    // And with the first of them taken into account.
    const touch = await fingers(page);
    await touch.drag(await at(page, 300, 300), await at(page, 300, 200));
    await expect.poll(async () => (await stats(page)).scroll.top).toBeCloseTo(92, 3);
    expect(await isLoaded()).toBe(true);
  });

  test("a finger opens a filter by its button, as a click does", async ({ page }) => {
    await page.getByTestId("m3a-filter-btn").click();
    await settled(page);
    const touch = await fingers(page);
    const { metrics } = await stats(page);
    await touch.tap(await at(page, metrics.colWidth * 2 - 12, metrics.rowHeight / 2));
    await expect(page.locator("dialog[open]")).toBeVisible();
  });
});

test.describe("a workbook that is shared", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "the rules are the same everywhere");

  let server: CollabServerHandle | null = null;

  test.beforeAll(async () => {
    server = await startCollabServer();
  });

  test.afterAll(async () => {
    await server?.stop();
    server = null;
  });

  test("is read on a phone, whatever the link is for", async ({ page, browser }) => {
    if (!server) {
      throw new Error("the server has not been started");
    }
    await signInDev(page, server.baseUrl, "alice");
    const { joinPath } = await createAndShareSheet(page, "To be read on a phone");
    await editCell(page, 0, 0, "A1", "100");
    const context = await browser.newContext({
      viewport: { width: 360, height: 640 },
      hasTouch: true,
      locale: "en-US",
    });
    try {
      const phone = await context.newPage();
      await joinViaSharePath(phone, server.baseUrl, joinPath, "bob");
      await phone.waitForFunction(() => window.__reticula?.textAt(0, 0) === "100");
      await expect(phone.locator(".app")).toHaveAttribute("data-touch", "true");
      await isLookedAtOnly(phone);
      const before = await content(phone);
      const cell = await middleOf(phone, 0, 0);
      await phone.touchscreen.tap(cell.x, cell.y);
      await phone.keyboard.type("7");
      await phone.keyboard.press("Enter");
      await phone.keyboard.press("Delete");
      expect(await content(phone)).toEqual(before);

      await phone.getByTestId("history-btn").tap();
      const drawer = phone.locator(".history-drawer");
      await expect(drawer.locator(".history-item").first()).toBeVisible();
      const box = await drawer.boundingBox();
      expect(box?.x).toBeGreaterThanOrEqual(0);
      expect((box?.x ?? 0) + (box?.width ?? 0)).toBeLessThanOrEqual(360);
      expect(await drawer.evaluate((one) => one.scrollWidth <= one.clientWidth)).toBe(true);
      await expect(drawer.locator(".history-version-form")).toHaveCount(0);
      // The zoom is for what is looked at from here, too.
      const zoom = await phone.locator(".zoom-control").boundingBox();
      expect(zoom?.y).toBeGreaterThanOrEqual((box?.y ?? 0) + (box?.height ?? 0));
      await phone.getByTestId("history-btn").evaluate((button: HTMLElement) => button.click());

      // Nothing is asked of the analyst that would change the workbook.
      await phone.getByTestId("ai-panel-btn").tap();
      await expect(phone.getByTestId("ai-tab-demos")).toBeVisible();
      await expect(phone.getByTestId("ai-tab-cloud")).toHaveCount(0);
    } finally {
      await context.close();
    }
  });
});
