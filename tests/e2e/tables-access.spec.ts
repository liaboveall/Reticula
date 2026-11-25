import { expect, type Page, test } from "@playwright/test";
import { activate, expectNamed, tabTo } from "./keyboard-helpers.ts";
import {
  canKeepTables,
  expectSaid,
  goTo,
  importTable,
  listenToPage,
  openPalette,
  openWorkbook,
  ordersCsv,
  runCommand,
  selectedCell,
  tab,
  tableGrid,
  textsOf,
  view,
} from "./tables-helpers.ts";

// Data tables with the keyboard alone, with what is read out, in Chinese, with
// fingers, in other colors and in narrow windows.

async function openOrSkip(page: Page, query = ""): Promise<void> {
  await listenToPage(page);
  await openWorkbook(page, query);
  test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
}

test("everything there is to do with a table can be done with the keyboard", async ({ page }) => {
  await openOrSkip(page);
  const keys = page.keyboard;

  // A file is imported.
  page.once("filechooser", () => {});
  await openPalette(page);
  await keys.type("import as data table");
  await keys.press("Enter");
  await expect(page.getByTestId("table-import-choose")).toBeFocused();
  await page.getByTestId("table-import-input").setInputFiles({
    name: "orders.csv",
    mimeType: "text/csv",
    buffer: Buffer.from(ordersCsv(1_000)),
  });
  await expect(page.getByTestId("table-import-preview")).toBeVisible();
  await expectNamed(page.getByTestId("table-import"));
  await tabTo(page, page.getByTestId("table-import-encoding"));
  await tabTo(page, page.getByTestId("table-import-header"));
  await keys.press("Space");
  await expect(page.getByTestId("table-import-header")).not.toBeChecked();
  await keys.press("Space");
  await tabTo(page, page.getByTestId("table-import-type-1"));
  await tabTo(page, page.getByTestId("table-import-start"));
  await keys.press("Enter");
  await expect(tableGrid(page)).toBeFocused();
  await expect(view(page)).toHaveAttribute("data-status", "open");

  // It is sorted from the menu of a column, which opens on its first entry.
  await keys.press("ArrowRight");
  await keys.press("ArrowRight");
  await keys.press("Alt+ArrowDown");
  const menu = page.getByTestId("table-menu");
  await expect(menu.getByTestId("menu-copy")).toBeFocused();
  await expectNamed(menu);
  await keys.press("ArrowDown");
  await keys.press("ArrowDown");
  await expect(menu.getByTestId("menu-tableSortDescending")).toBeFocused();
  await keys.press("Enter");
  await expectSaid(page, /Sorted by amount, descending/);
  await expect(tableGrid(page)).toBeFocused();
  await keys.press("Shift+F10");
  await expect(menu).toBeVisible();
  await keys.press("Escape");
  await expect(tableGrid(page)).toBeFocused();

  // It is filtered by values.
  await keys.press("ArrowLeft");
  await keys.press("Alt+ArrowDown");
  for (let presses = 0; presses < 5; presses++) {
    await keys.press("ArrowDown");
  }
  await expect(menu.getByTestId("menu-tableFilterValues")).toBeFocused();
  await keys.press("Enter");
  await expect(page.getByTestId("table-values-search")).toBeFocused();
  await expectNamed(page.getByTestId("table-filter"));
  await activate(page, page.getByTestId("table-values-none"));
  await tabTo(page, page.getByLabel(/^North/));
  await keys.press("Space");
  await keys.press("Enter");
  await expect(page.getByTestId("table-count")).toHaveText("250 of 1,000 rows");
  await expect(tableGrid(page)).toBeFocused();

  // And by a condition, by way of the list of commands.
  await keys.press("ArrowRight");
  await openPalette(page);
  await keys.type("by a condition");
  await keys.press("Enter");
  await expect(page.getByTestId("table-condition")).toBeFocused();
  await expectNamed(page.getByTestId("table-filter"));
  await page.getByTestId("table-condition").selectOption("number:less");
  await tabTo(page, page.getByTestId("table-value-a"));
  await keys.type("100");
  await keys.press("Enter");
  await expect(page.getByTestId("table-count")).toHaveText(/^\d\d of 1,000 rows$/);

  // The panel has the focus when it opens, and takes away what the rows are filtered by.
  await runCommand(page, "tablePanel");
  const panel = page.getByTestId("table-panel");
  await expectNamed(panel);
  await activate(page, panel.getByRole("button", { name: "Do not filter by amount" }));
  await expect(page.getByTestId("table-count")).toHaveText("250 of 1,000 rows");
  await activate(page, panel.getByRole("button", { name: "Do not sort by amount" }));
  await expectSaid(page, /Not sorted any more/);
  await keys.press("Escape");
  await expect(tableGrid(page)).toBeFocused();

  // Columns are frozen, fitted and sized.
  for (const [words, said] of [
    ["freeze first column", /Columns frozen: 1/],
    ["no freeze", /No columns frozen/],
  ] as const) {
    await openPalette(page);
    await keys.type(words);
    await keys.press("Enter");
    await expectSaid(page, said);
  }
  await keys.press("Shift+F6");
  await keys.press("F6");
  await expect(tableGrid(page)).toBeFocused();

  // It is renamed, moved and deleted from its tab.
  await keys.press("F6");
  await expect(tab(page, "orders")).toBeFocused();
  await keys.press("F2");
  await expect(page.getByTestId("table-rename-input")).toBeFocused();
  await keys.type("sold");
  await keys.press("Enter");
  await expect(tab(page, "sold")).toBeFocused();
  await keys.press("Shift+F10");
  await expect(page.getByTestId("table-tab-menu")).toBeVisible();
  await keys.press("Escape");
  await keys.press("Delete");
  await expect(page.getByTestId("table-delete")).toBeVisible();
  await tabTo(page, page.getByTestId("table-delete-confirm"));
  await keys.press("Enter");
  await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
});

test("the menus and dialogs of a table close into the grid again", async ({ page }) => {
  await openOrSkip(page);
  await importTable(page, "orders", ordersCsv(100));
  for (const id of ["tableFilterValues", "tableFilterCondition", "tableRename", "tableDelete"]) {
    await runCommand(page, id);
    await expect(page.locator("dialog[open]")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator("dialog[open]")).toHaveCount(0);
    await expect(tableGrid(page)).toBeFocused();
  }
});

test("all of it is in Chinese", async ({ page }) => {
  await openOrSkip(page, "&lang=zh");
  page.once("filechooser", () => {});
  await openPalette(page);
  // The texts of tables come after those of the first screen.
  await expect(page.getByTestId("command-importTable")).toContainText("导入为数据表");
  await page.keyboard.type("导入为数据表");
  await page.keyboard.press("Enter");
  const dialog = page.getByTestId("table-import");
  await expect(dialog.getByRole("heading")).toHaveText("导入为数据表");
  await page.getByTestId("table-import-input").setInputFiles({
    name: "订单.csv",
    mimeType: "text/csv",
    buffer: Buffer.from(ordersCsv(1_000)),
  });
  await expect(page.getByTestId("table-import-type-3")).toContainText("自动：日期");
  await page.getByTestId("table-import-start").click();
  await expect(page.getByTestId("table-count")).toHaveText("1,000 行");
  await expectSaid(page, /已导入 订单：1,000 行/);
  await goTo(page, "amount 1");
  await page.keyboard.press("Alt+ArrowDown");
  await page.getByTestId("menu-tableSortDescending").click();
  await expectSaid(page, /已排序：amount，降序/);
  await runCommand(page, "tablePanel");
  await expect(page.getByTestId("table-rows")).toHaveText("显示 1,000 行中的 1,000 行");
  await expect(page.getByTestId("table-panel").getByRole("columnheader").first()).toHaveText("列");
  await page.evaluate(() => window.__reticula?.crashDataWorker());
  await expect(page.getByTestId("table-problem")).toContainText(
    "显示数据表的部分已停止。工作簿的其余部分不受影响。",
  );
  await expect(page.getByTestId("table-reopen")).toHaveText("重新打开");
});

test("with fingers a table can be looked at, sorted and filtered, and not be changed", async ({
  page,
  browserName,
}) => {
  await openOrSkip(page);
  await importTable(page, "orders", ordersCsv(1_000));
  await page.setViewportSize({ width: 844, height: 390 });
  await openWorkbook(page, "&touch=1");
  await expect(view(page)).toHaveAttribute("data-status", "open");
  await expect(page.getByTestId("touch-bar")).toBeHidden();
  const box = await tableGrid(page).boundingBox();
  if (!box) {
    throw new Error("the grid has no place");
  }
  // The button of the first column.
  await page.mouse.click(box.x + 64 + 112 - 12, box.y + 12);
  await expect(page.getByTestId("table-menu")).toBeVisible();
  await page.getByTestId("menu-tableSortDescending").click();
  await expect
    .poll(() => selectedCell(page))
    .toMatchObject({ column: "id", row: "1", text: "1000" });
  await page.getByTestId("table-panel-btn").click();
  await expect(page.getByTestId("table-panel")).toBeVisible();
  await page.getByTestId("table-panel").getByRole("button", { name: "Close" }).click();
  await page.getByRole("tab", { name: "orders" }).click({ button: "right" });
  await expect(page.getByTestId("menu-tableRename")).toHaveAttribute("aria-disabled", "true");
  await expect(page.getByTestId("menu-tableDelete")).toHaveAttribute("aria-disabled", "true");
  await page.keyboard.press("Escape");
  if (browserName === "chromium") {
    // Wheels and drags go through the same code as for worksheets, which has tests of its own.
    await page.mouse.wheel(0, 6_000);
  }
});

for (const media of [
  { name: "dark", options: { colorScheme: "dark" } },
  { name: "high contrast", options: { forcedColors: "active" } },
] as const) {
  test(`in ${media.name} a table and its dialogs are drawn`, async ({ page }) => {
    await page.emulateMedia(media.options);
    await openOrSkip(page);
    await importTable(page, "orders", ordersCsv(100));
    await goTo(page, "region 2");
    await page.keyboard.press("Alt+ArrowDown");
    await page.getByTestId("menu-tableFilterValues").click();
    await expect(page.getByTestId("table-filter")).toBeVisible();
    await page.keyboard.press("Escape");
    await runCommand(page, "tablePanel");
    // The first column of the panel stays over the others when they are scrolled.
    const background = await page
      .getByTestId("table-column-0")
      .locator("th")
      .evaluate((cell) => getComputedStyle(cell).backgroundColor);
    expect(background).not.toBe("rgba(0, 0, 0, 0)");
    expect(await textsOf(page, "region", [2])).toEqual(["East"]);
  });
}

test("at every zoom there is the same table", async ({ page }) => {
  await openOrSkip(page);
  await importTable(page, "orders", ordersCsv(1_000));
  for (const percent of [25, 50, 150, 200]) {
    await runCommand(page, `zoom:${percent}`);
    await expect(page.getByTestId("zoom-value")).toHaveText(`${percent}%`);
    await goTo(page, "amount 500");
    await expect.poll(() => selectedCell(page)).toMatchObject({ row: "500", text: "595.00" });
    await goTo(page, "id 1");
  }
});

test("in a narrow window the panel is over the table, and can be closed", async ({ page }) => {
  await page.setViewportSize({ width: 480, height: 700 });
  await openOrSkip(page);
  await importTable(page, "orders", ordersCsv(100));
  await page.getByTestId("table-panel-btn").click();
  const panel = page.getByTestId("table-panel");
  const box = await panel.boundingBox();
  expect(box?.width).toBe(480);
  await panel.getByRole("button", { name: "Close" }).click();
  await expect(panel).toBeHidden();
  await expect(page.getByTestId("table-panel-btn")).toBeFocused();
  // Nothing of the window is wider than it is.
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(
    true,
  );
});

test("the built-in table is made when its address is opened, once", async ({ page }) => {
  await page.goto("/app?hooks&doc=orders-10m&sample=20000");
  await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
  test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
  await expect(view(page)).toHaveAttribute("data-status", "open", { timeout: 15_000 });
  await expect(page.getByTestId("table-count")).toHaveText("20,000 rows");
  expect(new URL(page.url()).searchParams.has("sample")).toBe(false);
  await page.goto("/app?hooks&doc=orders-10m&sample=20000");
  await expect(page.getByTestId("table-count")).toHaveText("20,000 rows");
  await expect(page.getByRole("tab")).toHaveText(["Sheet1", "20,000 orders"]);
});

test("an address that asks for no number of rows there could be asks for nothing", async ({
  page,
}) => {
  for (const rows of ["-5", "0", "1.5", "1e12", "Infinity", "100000001", "many"]) {
    await page.goto(`/app?hooks&doc=orders-10m&sample=${rows}`);
    await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
    await expect.poll(() => new URL(page.url()).searchParams.has("sample")).toBe(false);
    await page.waitForTimeout(300);
    await expect(page.getByTestId("table-importing")).toBeHidden();
    await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
  }
});

test("the landing page and the list of workbooks lead to the built-in table", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByTestId("open-sample-table")).toHaveAttribute(
    "href",
    "/app?doc=orders-10m&sample=10000000",
  );
  await page.goto("/workbooks");
  await page.getByTestId("workbooks-tab-samples").click();
  await expect(page.getByTestId("open-sample-table")).toBeVisible();
});
