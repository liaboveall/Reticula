import { expect, type Page, test } from "@playwright/test";

// What the pointer does to the grid when things do not go as usual: a second
// pointer, a cancelled one, an edge that is pressed and not moved, Escape.

test.skip(({ browserName }) => browserName !== "chromium", "the rules are the same everywhere");

const MOUSE = 1;

async function settled(page: Page): Promise<void> {
  await page.waitForFunction(() => {
    const stats = window.__reticula?.stats();
    return stats !== undefined && stats.cellsDrawn > 0 && stats.missingCells === 0;
  });
}

async function open(page: Page) {
  // One of them reads what is drawn back, from a 2D canvas.
  await page.goto("/app?hooks&renderer=canvas2d");
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await settled(page);
  const box = await page.getByTestId("grid").boundingBox();
  if (!box) {
    throw new Error("the grid has no box");
  }
  const { metrics } = await stats(page);
  return {
    /** A point of the page by its distance from the corner of the first cell. */
    at: (x: number, y: number) => ({
      x: box.x + metrics.headerWidth + x,
      y: box.y + metrics.headerHeight + y,
    }),
    colWidth: metrics.colWidth,
    rowHeight: metrics.rowHeight,
  };
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

/** Operations the engine has recorded; asking for its digest makes sure that it has dealt with all of them. */
function operations(page: Page): Promise<number> {
  return page.evaluate(async () => {
    await window.__reticula?.stateDigest();
    return Number(window.__reticula?.docStatus().walSeq ?? "0");
  });
}

function nextFrames(page: Page): Promise<void> {
  return page.evaluate(
    () =>
      new Promise<void>((done) => requestAnimationFrame(() => requestAnimationFrame(() => done()))),
  );
}

/** The column under a point of the header, found out by a click on it. */
async function columnAt(page: Page, point: { x: number; y: number }): Promise<number | undefined> {
  await page.mouse.click(point.x, point.y);
  return (await stats(page)).selection?.range.firstCol;
}

/** Tells the grid about a pointer the way a browser would. */
function pointer(
  page: Page,
  type: "pointerdown" | "pointermove" | "pointerup" | "pointercancel",
  pointerId: number,
  point: { x: number; y: number },
): Promise<void> {
  return page.evaluate(
    ({ type, pointerId, point }) => {
      document.querySelector(".reticula-input")?.dispatchEvent(
        new PointerEvent(type, {
          pointerId,
          pointerType: "touch",
          clientX: point.x,
          clientY: point.y,
          bubbles: true,
          cancelable: true,
        }),
      );
    },
    { type, pointerId, point },
  );
}

test("the pointer is listened to above the picture, which assistive technology passes over", async ({
  page,
}) => {
  const { at } = await open(page);
  const inside = at(20, 20);
  const layers = await page.evaluate(({ x, y }) => {
    const grid = document.querySelector('[data-testid="grid"]');
    return {
      under: document.elementFromPoint(x, y)?.className,
      order: [...(grid?.children ?? [])].map((child) =>
        child instanceof HTMLCanvasElement ? "canvas" : child.className,
      ),
      hidden: [...(grid?.children ?? [])]
        .filter((child) => child.getAttribute("aria-hidden") === "true")
        .map((child) => (child instanceof HTMLCanvasElement ? "canvas" : child.className)),
    };
  }, inside);
  expect(layers.under).toBe("reticula-input");
  expect(layers.order).toEqual([
    "canvas",
    "reticula-input",
    "reticula-scrollbar",
    "reticula-scrollbar",
    "reticula-scrollbar-corner",
    "reticula-editor",
  ]);
  expect(layers.hidden).toEqual([
    "canvas",
    "reticula-scrollbar",
    "reticula-scrollbar",
    "reticula-scrollbar-corner",
  ]);
});

test("an edge that is pressed and let go without moving resizes nothing", async ({ page }) => {
  const { at, colWidth, rowHeight } = await open(page);
  const before = await operations(page);
  const edge = at(colWidth, -10);
  await page.mouse.move(edge.x, edge.y);
  await page.mouse.down();
  await page.mouse.up();
  const rowEdge = at(-20, rowHeight);
  await page.mouse.move(rowEdge.x, rowEdge.y);
  await page.mouse.down();
  await page.mouse.up();
  expect(await operations(page)).toBe(before);
});

test("a column is as wide as it was when its edge is moved away and back", async ({ page }) => {
  const { at, colWidth } = await open(page);
  const before = await operations(page);
  const edge = at(colWidth, -10);
  await page.mouse.move(edge.x, edge.y);
  await page.mouse.down();
  await page.mouse.move(edge.x + 60, edge.y, { steps: 3 });
  await page.mouse.move(edge.x, edge.y, { steps: 3 });
  await page.mouse.up();
  expect(await operations(page)).toBe(before);
});

test("dragging an edge is one operation, and double-clicking it is another", async ({ page }) => {
  const { at, colWidth } = await open(page);
  const before = await operations(page);
  const edge = at(colWidth, -10);
  await page.mouse.move(edge.x, edge.y);
  await page.mouse.down();
  await page.mouse.move(edge.x + 80, edge.y, { steps: 4 });
  await page.mouse.up();
  await expect.poll(() => operations(page)).toBe(before + 1);
  expect(await columnAt(page, at(colWidth + 40, -10))).toBe(0);

  await page.mouse.dblclick(edge.x + 80, edge.y);
  await expect.poll(() => operations(page)).toBe(before + 2);
  // The column is as wide as its eight digits need.
  expect(await columnAt(page, at(colWidth, -10))).toBe(1);
  await nextFrames(page);
  expect(await operations(page)).toBe(before + 2);
});

for (const ending of ["Escape", "a cancelled pointer", "a capture that is lost"] as const) {
  test(`${ending} takes a resize back`, async ({ page }) => {
    const { at, colWidth } = await open(page);
    const before = await operations(page);
    await page.mouse.click(at(20, 20).x, at(20, 20).y);
    const edge = at(colWidth, -10);
    await page.mouse.move(edge.x, edge.y);
    await page.mouse.down();
    await page.mouse.move(edge.x + 300, edge.y, { steps: 3 });
    await nextFrames(page);
    const during = (await stats(page)).visible.endCol;

    if (ending === "Escape") {
      await page.keyboard.press("Escape");
    } else if (ending === "a cancelled pointer") {
      await pointer(page, "pointercancel", MOUSE, edge);
    } else {
      await page.evaluate(
        (id) => document.querySelector(".reticula-input")?.releasePointerCapture(id),
        MOUSE,
      );
      await page.mouse.move(edge.x + 301, edge.y);
    }
    await nextFrames(page);
    expect((await stats(page)).visible.endCol).toBeGreaterThan(during);
    await page.mouse.move(edge.x + 350, edge.y, { steps: 2 });
    await page.mouse.up();
    await nextFrames(page);
    expect(await operations(page)).toBe(before);
    expect(await columnAt(page, at(colWidth + 40, -10))).toBe(1);
  });
}

test("Escape takes a fill back and keeps the selection", async ({ page }) => {
  const { at, colWidth, rowHeight } = await open(page);
  const before = await operations(page);
  await page.mouse.click(at(colWidth * 3.5, rowHeight * 2.5).x, at(0, rowHeight * 2.5).y);
  const selected = (await stats(page)).selection?.name;
  const handle = at(colWidth * 4, rowHeight * 3);
  await page.mouse.move(handle.x, handle.y);
  await page.mouse.down();
  await page.mouse.move(handle.x - 20, handle.y + rowHeight * 4, { steps: 4 });
  await page.keyboard.press("Escape");
  await page.mouse.up();
  await nextFrames(page);
  expect(await operations(page)).toBe(before);
  expect((await stats(page)).selection?.name).toBe(selected);
  // The next Escape is an ordinary one, and the next drag selects.
  await page.keyboard.press("Escape");
  await page.mouse.move(at(10, 10).x, at(10, 10).y);
  await page.mouse.down();
  await page.mouse.move(at(colWidth * 1.5, rowHeight * 1.5).x, at(0, rowHeight * 1.5).y);
  await page.mouse.up();
  expect((await stats(page)).selection?.name).toBe("A1:B2");
});

test("a second pointer does not take a drag over", async ({ page }) => {
  const { at, colWidth, rowHeight } = await open(page);
  const start = at(colWidth * 1.5, rowHeight * 1.5);
  const end = at(colWidth * 2.5, rowHeight * 3.5);
  const other = at(colWidth * 5.5, rowHeight * 9.5);
  await page.mouse.move(start.x, start.y);
  await page.mouse.down();
  await page.mouse.move(end.x, end.y, { steps: 3 });
  await pointer(page, "pointerdown", 77, other);
  await pointer(page, "pointermove", 77, other);
  await pointer(page, "pointerup", 77, other);
  expect((await stats(page)).selection?.name).toBe("B2:C4");
  await page.mouse.move(end.x + colWidth, end.y, { steps: 2 });
  await page.mouse.up();
  expect((await stats(page)).selection?.name).toBe("B2:D4");
});

test("the picture is drawn again when the canvas has its context back", async ({ page }) => {
  await open(page);
  const result = await page.evaluate(async () => {
    const canvas = document.querySelector<HTMLCanvasElement>('[data-testid="grid"] canvas');
    const context = canvas?.getContext("2d");
    const hooks = window.__reticula;
    if (!canvas || !context || !hooks) {
      throw new Error("the grid has no 2D canvas");
    }
    const ink = () => {
      const { data } = context.getImageData(0, 0, canvas.width, canvas.height);
      let opaque = 0;
      for (let at = 3; at < data.length; at += 4) {
        opaque += data[at] === 255 ? 1 : 0;
      }
      return opaque / (canvas.width * canvas.height);
    };
    const before = ink();
    const lost = new Event("contextlost", { cancelable: true });
    canvas.dispatchEvent(lost);
    context.reset();
    const cleared = ink();
    const { frames } = hooks.stats();
    canvas.dispatchEvent(new Event("contextrestored"));
    while (hooks.stats().frames === frames) {
      await new Promise(requestAnimationFrame);
    }
    return { before, cleared, after: ink(), prevented: lost.defaultPrevented };
  });
  expect(result).toEqual({ before: 1, cleared: 0, after: 1, prevented: false });
});
