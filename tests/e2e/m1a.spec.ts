import { expect, type Page, test } from "@playwright/test";
import { fillFormulaBar } from "./formula-bar-helpers.ts";

async function settled(page: Page): Promise<void> {
  await page.waitForFunction(() => {
    const stats = window.__reticula?.stats();
    return stats !== undefined && stats.cellsDrawn > 0 && stats.missingCells === 0;
  });
}

async function openEditor(page: Page): Promise<void> {
  await page.goto("/app?hooks");
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await settled(page);
}

test("landing page displays archived metrics and navigates to the 1M-row live demo", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.getByTestId("landing-page")).toBeVisible();
  await expect(page.locator("h1")).toContainText("One million rows");

  await page.getByTestId("open-demo").click();
  await expect(page).toHaveURL(/\/app$/);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await expect(page.getByTestId("document")).toContainText("1,000,000 rows × 22 columns");
});

test("editing V1 recalculates 100,001 dependent formulas and updates V2", async ({ page }) => {
  await openEditor(page);

  // Jump to V1 (`row = 0, col = 21`) and inspect V2 (`row = 1, col = 21`) first.
  await page.evaluate(() => {
    window.__reticula?.select(1, 21);
    window.__reticula?.scrollTo({ left: Number.MAX_SAFE_INTEGER, top: 0 });
  });
  await settled(page);
  await expect(page.getByTestId("name-box")).toHaveText("V2");
  await expect(page.getByTestId("formula-input")).toHaveText("=SUM(M2:M100001)");
  const initialSum = await page.evaluate(() => window.__reticula?.textAt(1, 21));
  expect(initialSum).toMatch(/^\d+\.\d\d$/);

  // Select V1 via the quick-jump button, edit via formula bar to `5%`, and commit.
  await page.getByTestId("jump-v1").click();
  await expect(page.getByTestId("name-box")).toHaveText("V1");
  await fillFormulaBar(page, "5%");

  await expect(page.getByTestId("recalc-status")).toContainText("recalculated 100,001 cells in");
  await settled(page);

  await page.waitForFunction((before) => {
    const next = window.__reticula?.textAt(1, 21);
    return next !== null && next !== undefined && next !== before;
  }, initialSum);
  const updatedSum = await page.evaluate(() => window.__reticula?.textAt(1, 21));
  expect(Number(updatedSum)).toBeLessThan(Number(initialSum));
});

test("inserting and deleting rows updates row count and shifts bound formula addresses", async ({
  page,
}) => {
  await openEditor(page);

  // Select row 2 (`row = 1, col = 0`) and insert a row above it (between row 1 and row 2).
  await page.evaluate(() => window.__reticula?.select(0, 0));
  await page.getByTestId("insert-row-above").click();
  await expect(page.getByTestId("document")).toContainText("1,000,001 rows × 22 columns");
  await expect(page.getByTestId("recalc-status")).toContainText("inserted rows in");

  // Original V2 (`RowId(1)`) is now at row index 2 (`V3`), and its bound range shifted to `M3:M100002`!
  await page.evaluate(() => {
    window.__reticula?.select(2, 21);
    window.__reticula?.scrollTo({ left: Number.MAX_SAFE_INTEGER, top: 0 });
  });
  await settled(page);
  await expect(page.getByTestId("name-box")).toHaveText("V3");
  await expect(page.getByTestId("formula-input")).toHaveText("=SUM(M3:M100002)");

  // Delete the newly inserted top row (`row = 0`).
  await page.evaluate(() => {
    window.__reticula?.scrollTo({ left: 0, top: 0 });
    window.__reticula?.select(0, 0);
  });
  await settled(page);
  await page.getByTestId("delete-rows").click();
  await expect(page.getByTestId("document")).toContainText("1,000,000 rows × 22 columns");
});

test("range selection, Delete clear, and invalid formula rejection work cleanly", async ({
  page,
}) => {
  await openEditor(page);

  // Click cell B2 (`row = 1, col = 1`) so keyboard focus is inside CellEditor.
  const grid = page.getByTestId("grid");
  const box = await grid.boundingBox();
  if (!box) {
    throw new Error("missing grid bounding box");
  }
  const metrics = await page.evaluate(() => window.__reticula?.stats().metrics);
  if (!metrics) {
    throw new Error("missing metrics");
  }
  await page.mouse.click(
    box.x + metrics.headerWidth + metrics.colWidth * 1.5,
    box.y + metrics.headerHeight + metrics.rowHeight * 1.5,
  );
  await expect(page.getByTestId("name-box")).toHaveText("B2");

  // Extend selection with Shift+ArrowRight and Shift+ArrowDown -> B2:C3.
  await page.keyboard.press("Shift+ArrowRight");
  await page.keyboard.press("Shift+ArrowDown");
  await expect(page.getByTestId("name-box")).toHaveText("B2:C3");

  // Press Delete to clear B2:C3.
  await page.keyboard.press("Delete");
  await page.waitForFunction(() => window.__reticula?.textAt(1, 1) === "");
  expect(await page.evaluate(() => window.__reticula?.textAt(2, 2))).toBe("");

  // Malformed formula shows rejection alert without breaking the sheet.
  await fillFormulaBar(page, "=SUM(1,");
  await expect(page.getByTestId("rejected-banner")).toBeVisible();
});

test("narrow viewport on /app shows mobile fallback with escape hatch", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/app");
  await expect(page.getByTestId("mobile-fallback")).toBeVisible();
  await expect(page.getByTestId("mobile-fallback")).toContainText(
    "please open this link on desktop",
  );

  await expect(page.getByTestId("open-anyway")).toHaveText("Open editor anyway");
  await page.getByTestId("open-anyway").click();
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
});
