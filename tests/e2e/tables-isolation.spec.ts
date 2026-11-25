import { expect, type Page, test } from "@playwright/test";
import {
  canKeepTables,
  columnMenu,
  content,
  DOC,
  expectSaid,
  goTo,
  importTable,
  listenToPage,
  openPalette,
  openWorkbook,
  ordersCsv,
  selectedCell,
  tab,
  tableGrid,
  textsOf,
  view,
} from "./tables-helpers.ts";

// A data table and the worksheets beside it leave each other alone, whatever
// keys are pressed and whatever fails.

test.beforeEach(async ({ page }) => {
  await listenToPage(page);
  await openWorkbook(page);
  test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
});

async function fillSheet(page: Page): Promise<void> {
  await page.getByRole("textbox", { name: "Cell editor" }).focus();
  for (const text of ["1", "2", "=A1+A2", "four"]) {
    await page.keyboard.type(text);
    await page.keyboard.press("Enter");
  }
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(2, 0))).toBe("3");
  await page.keyboard.press("ControlOrMeta+z");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(3, 0))).toBe("");
}

test.describe("while a table is in view", () => {
  test("no key changes anything in the worksheets", async ({ page, context, browserName }) => {
    if (browserName === "chromium") {
      await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    }
    await fillSheet(page);
    await importTable(page, "orders", ordersCsv(100));
    const before = await content(page);
    await goTo(page, "region 3");
    for (const key of [
      "Delete",
      "Backspace",
      "x",
      "F2",
      "y",
      "Enter",
      "=",
      "Escape",
      "ControlOrMeta+c",
      "ControlOrMeta+x",
      "ControlOrMeta+v",
      "ControlOrMeta+z",
      "ControlOrMeta+y",
      "ControlOrMeta+Shift+z",
      "ControlOrMeta+d",
      "ControlOrMeta+r",
      "ControlOrMeta+b",
      "ControlOrMeta+i",
      "ControlOrMeta+5",
      "ControlOrMeta+f",
      "ControlOrMeta+h",
      "Shift+Space",
      "ControlOrMeta+Space",
    ]) {
      await page.keyboard.press(key);
    }
    await expect(page.locator("dialog[open]")).toHaveCount(0);
    await expect.poll(() => selectedCell(page)).toMatchObject({ row: "3" });
    await tab(page, "orders").focus();
    await page.keyboard.press("Delete");
    await page.getByTestId("table-delete").getByRole("button", { name: "Cancel" }).click();
    await page.keyboard.press("F2");
    await expect(page.getByTestId("table-rename-input")).toBeFocused();
    await page.keyboard.press("Escape");
    // The menu of a worksheet's tab has nothing to change it with either.
    await tab(page, "Sheet1").click({ button: "right" });
    for (const id of ["ctx-rename-sheet", "ctx-delete-sheet"]) {
      await expect(page.getByTestId(id)).toHaveAttribute("aria-disabled", "true");
    }
    await page.keyboard.press("Escape");
    for (const id of ["filter", "sort", "insertRowAbove", "deleteCols", "bold", "find"]) {
      await openPalette(page);
      await expect(page.getByTestId(`command-${id}`)).toHaveAttribute("aria-disabled", "true");
      await page.keyboard.press("Escape");
    }
    await expect(page.getByTestId("undo-btn")).toHaveAttribute("aria-disabled", "true");
    expect(await content(page)).toEqual(before);

    await tab(page, "Sheet1").click();
    await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(2, 0))).toBe("3");
    await page.keyboard.press("ControlOrMeta+y");
    await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(3, 0))).toBe("four");
  });

  test("what was being typed is in the worksheet, a formula too", async ({ page }) => {
    await importTable(page, "orders", ordersCsv(100));
    await tab(page, "Sheet1").click();
    await page.keyboard.type("typed");
    await tab(page, "orders").click();
    await expect(view(page)).toHaveAttribute("data-status", "open");
    await tab(page, "Sheet1").click();
    await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("typed");
    await page.evaluate(() => window.__reticula?.select(1, 1));
    await page.getByRole("textbox", { name: "Cell editor" }).focus();
    await page.keyboard.type("=1+2");
    await tab(page, "orders").click();
    await expect(tableGrid(page)).toBeFocused();
    await expect(view(page)).toHaveAttribute("data-status", "open");
    await page.keyboard.press("ArrowDown");
    await expect.poll(() => selectedCell(page)).toMatchObject({ row: "2" });
    await tab(page, "Sheet1").click();
    await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 1))).toBe("3");
  });

  test("a past or a drafted workbook closes it, and it can be opened again after", async ({
    page,
  }) => {
    await importTable(page, "orders", ordersCsv(100));
    await page.getByTestId("ai-panel-btn").click();
    await page.getByTestId("ai-demo-regional-sales-summary").click();
    await expect(page.getByTestId("ai-draft-banner")).toBeVisible();
    await expect(view(page)).toBeHidden();
    await expect(tab(page, "orders")).toBeHidden();
    await page.getByTestId("ai-draft-banner").getByRole("button").first().click();
    await expect(page.getByTestId("ai-draft-banner")).toBeHidden();
    await tab(page, "orders").click();
    await expect(view(page)).toHaveAttribute("data-status", "open");
  });

  test("the worksheet's grid asks for nothing, and goes on where it was", async ({ page }) => {
    await fillSheet(page);
    await page.evaluate(() => window.__reticula?.scrollTo({ top: 24_000 }));
    await importTable(page, "orders", ordersCsv(100));
    const paused = await page.evaluate(() => window.__reticula?.stats());
    await page.keyboard.press("ControlOrMeta+End");
    await page.waitForTimeout(300);
    const later = await page.evaluate(() => window.__reticula?.stats());
    expect(later?.epoch).toBe(paused?.epoch);
    expect(later?.pagesHeld).toBe(0);
    await tab(page, "Sheet1").click();
    await expect
      .poll(() => page.evaluate(() => window.__reticula?.stats().pagesHeld ?? 0))
      .toBeGreaterThan(0);
    expect((await page.evaluate(() => window.__reticula?.stats()))?.scroll.top).toBe(24_000);
  });
});

test.describe("a crash", () => {
  test("is told of, and leaves the worksheets and their engine alone", async ({ page }) => {
    await fillSheet(page);
    await importTable(page, "orders", ordersCsv(100));
    await goTo(page, "amount 50");
    const before = await selectedCell(page);
    await page.evaluate(() => window.__reticula?.crashDataWorker());
    const problem = page.getByTestId("table-problem");
    await expect(problem).toHaveAttribute("data-code", "crashed");
    await expect(problem).toContainText("The rest of the workbook is not affected.");
    await expect(page.getByTestId("table-reopen")).toBeFocused();
    await expect(page.getByTestId("engine-status")).toHaveText("ready");

    await tab(page, "Sheet1").click();
    await page.evaluate(() => window.__reticula?.select(5, 0));
    await page.keyboard.type("works");
    await page.keyboard.press("Enter");
    await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(5, 0))).toBe("works");

    // The first time, the table is opened again as if nothing had been.
    await tab(page, "orders").click();
    await expect(view(page)).toHaveAttribute("data-status", "open");
    await expect.poll(() => selectedCell(page)).toEqual(before);
  });

  test("that comes again has the table closed until it is asked for", async ({ page }) => {
    await importTable(page, "orders", ordersCsv(100));
    for (let times = 0; times < 2; times++) {
      await expect(view(page)).toHaveAttribute("data-status", "open");
      await page.evaluate(() => window.__reticula?.crashDataWorker());
      await expect(page.getByTestId("table-problem")).toBeVisible();
      await tab(page, "Sheet1").click();
      await tab(page, "orders").click();
    }
    await expect(page.getByTestId("table-problem")).toHaveAttribute("data-code", "crashed");
    await page.reload();
    await expect(page.getByTestId("table-problem")).toHaveAttribute("data-code", "crashed");
    const download = page.waitForEvent("download");
    await page.getByTestId("table-export").click();
    expect((await download).suggestedFilename()).toBe("orders.rtbl");
    await page.getByTestId("table-reopen").click();
    await expect(view(page)).toHaveAttribute("data-status", "open");
  });

  test("can be the end of a table, whose file can be deleted", async ({ page }) => {
    await importTable(page, "orders", ordersCsv(100));
    await page.evaluate(() => window.__reticula?.crashDataWorker());
    await page.getByTestId("table-delete").click();
    await page.getByTestId("table-delete-confirm").click();
    await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
  });
});

test("without enough memory a question is refused, and the rows stay as they were", async ({
  page,
}) => {
  await importTable(page, "orders", ordersCsv(20_000));
  await page.evaluate(() => window.__reticula?.tableMemoryLimit(4_096));
  await goTo(page, "amount 1");
  await columnMenu(page, "tableSortDescending");
  await expect(page.getByTestId("table-refused")).toHaveText(/There is not enough memory for this/);
  expect(await textsOf(page, "id", [1, 2])).toEqual(["1", "2"]);
  await page.evaluate(() => window.__reticula?.tableMemoryLimit(null));
  await goTo(page, "amount 1");
  await columnMenu(page, "tableSortDescending");
  await expectSaid(page, /Sorted by amount, descending/);
  await expect(page.getByTestId("table-refused")).toBeHidden();
});

test.describe("another tab", () => {
  test("that takes the workbook has its tables too", async ({ page, context }) => {
    await importTable(page, "orders", ordersCsv(100));
    const other = await context.newPage();
    await other.goto(`/app?hooks&doc=${DOC}`);
    await other.locator(".lock-overlay").getByRole("button").first().click();
    await expect(other.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
    await expect(page.locator(".lock-overlay")).toBeVisible();
    await expect(view(page)).toBeHidden();
    await other.getByRole("tab", { name: "orders" }).click();
    await expect(view(other)).toHaveAttribute("data-status", "open", { timeout: 10_000 });
    await expect(other.getByTestId("table-count")).toHaveText("100 rows");
  });

  test("that has the tables keeps them until they are asked for here", async ({
    page,
    context,
  }) => {
    await importTable(page, "orders", ordersCsv(100));
    const other = await context.newPage();
    await other.goto("/privacy");
    await page.reload();
    await expect(view(page)).toHaveAttribute("data-status", "open");
    await tab(page, "Sheet1").click();
    // What holds them somewhere else, in a worker that does not listen.
    await other.evaluate((name) => {
      void navigator.locks.request(name, { steal: true }, () => new Promise(() => {}));
    }, `reticula:tables:${DOC}`);
    await tab(page, "orders").click();
    await expect(page.getByTestId("table-problem")).toHaveAttribute(
      "data-code",
      /^(?:blocked|yielded)$/,
    );
    await page.getByTestId("table-take-over").click();
    await expect(view(page)).toHaveAttribute("data-status", "open", { timeout: 10_000 });
  });
});
