import { readFile } from "node:fs/promises";
import { expect, type Page, test } from "@playwright/test";
import {
  canKeepTables,
  columnMenu,
  expectSaid,
  filledCell,
  goTo,
  importTable,
  listenToPage,
  openWorkbook,
  ordersCsv,
  runCommand,
  selectedCell,
  sortOfSelected,
  tab,
  tableFiles,
  tableGrid,
  textsOf,
  view,
} from "./tables-helpers.ts";

// Data tables as they are worked with: their tabs, orders, filters, the panel.

test.beforeEach(async ({ page }) => {
  await listenToPage(page);
  await openWorkbook(page);
  test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
});

test.describe("a data table", () => {
  test("is imported, shown, and is there after a reload", async ({ page }) => {
    await importTable(page, "orders", ordersCsv(1_000));
    await expect(tab(page, "orders")).toHaveAttribute("aria-selected", "true");
    await expect(tableGrid(page)).toBeFocused();
    await expect(page.getByTestId("table-count")).toHaveText("1,000 rows");
    expect(await textsOf(page, "region", [1, 4])).toEqual(["south", "North"]);
    await goTo(page, "amount 900");
    await page.reload();
    await expect(view(page)).toHaveAttribute("data-status", "open");
    await expect
      .poll(() => selectedCell(page))
      .toMatchObject({ column: "amount", row: "900", text: "271.00" });
  });

  test("is gone to by rows and columns in the name box", async ({ page }) => {
    await importTable(page, "orders", ordersCsv(1_000));
    await goTo(page, "77");
    await expect.poll(() => selectedCell(page)).toMatchObject({ column: "id", row: "77" });
    await goTo(page, "C");
    await expect(page.getByTestId("name-box")).toHaveText("amount 1:amount 1000");
    await goTo(page, "note 5");
    await expect
      .poll(() => selectedCell(page))
      .toMatchObject({ column: "note", row: "5", text: "" });
    await page.keyboard.press("ControlOrMeta+g");
    await page.getByTestId("name-box-input").fill("1001");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("name-box-problem")).toHaveText(
      "“1001” is neither the number of a row nor a column.",
    );
    await page.keyboard.press("Escape");
    await expect(tableGrid(page)).toBeFocused();
  });

  test("is moved through with the keys, and read out with the names of its columns", async ({
    page,
  }) => {
    await importTable(page, "orders", ordersCsv(1_000));
    await page.keyboard.press("ArrowRight");
    await expectSaid(page, /region 1, south$/);
    await page.keyboard.press("ControlOrMeta+ArrowDown");
    await expectSaid(page, /region 1000, North$/);
    await page.keyboard.press("ControlOrMeta+End");
    await expectSaid(page, /note 1000, blank$/);
    await page.keyboard.press("ControlOrMeta+Home");
    await page.keyboard.press("Shift+ArrowRight");
    await expectSaid(page, /id 1 to region 1 selected, 2 cells$/);
    await page.keyboard.press("ControlOrMeta+Space");
    await expectSaid(page, /Columns id to region selected, 2 columns$/);
  });
});

test.describe("the tabs", () => {
  test.beforeEach(async ({ page }) => {
    await importTable(page, "orders", ordersCsv(20));
    await importTable(page, "returns", ordersCsv(30));
  });

  test("of tables come after those of worksheets, and the keys go through all of them", async ({
    page,
  }) => {
    await page.getByTestId("add-sheet-btn").click();
    await expect(tab(page, "Sheet2")).toHaveAttribute("aria-selected", "true");
    await expect(page.getByRole("tab")).toHaveText(["Sheet1", "Sheet2", "orders", "returns"]);
    await expect(view(page)).toBeHidden();
    await tab(page, "Sheet2").focus();
    await page.keyboard.press("ArrowRight");
    await expect(tab(page, "orders")).toBeFocused();
    await expect(tab(page, "orders")).toHaveAttribute("aria-selected", "true");
    await expect(page.getByTestId("table-count")).toHaveText("20 rows");
    await page.keyboard.press("End");
    await expect(tab(page, "returns")).toBeFocused();
    await expect(page.getByTestId("table-count")).toHaveText("30 rows");
    await page.keyboard.press("ArrowRight");
    await expect(tab(page, "Sheet1")).toBeFocused();
    await expect(view(page)).toBeHidden();
    await page.keyboard.press("ArrowLeft");
    await expect(tab(page, "returns")).toBeFocused();
    await page.keyboard.press("Home");
    await expect(tab(page, "Sheet1")).toHaveAttribute("aria-selected", "true");
    // The same from within the grids.
    await page.keyboard.press("Alt+PageDown");
    await page.keyboard.press("Alt+PageDown");
    await expect(tab(page, "orders")).toHaveAttribute("aria-selected", "true");
    await page.keyboard.press("Alt+PageUp");
    await expect(tab(page, "Sheet2")).toHaveAttribute("aria-selected", "true");
  });

  test("give the grid the focus when they are clicked, the table's or the worksheet's", async ({
    page,
  }) => {
    await tab(page, "orders").click();
    await expect(tableGrid(page)).toBeFocused();
    // The worksheet that was in view before: its tab is not the selected one.
    await tab(page, "Sheet1").click();
    await expect(view(page)).toBeHidden();
    await expect(page.getByRole("textbox", { name: "Cell editor" })).toBeFocused();
    await page.keyboard.type("12");
    await page.keyboard.press("Enter");
    await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("12");
  });

  test("rename tables, which have names of their own", async ({ page }) => {
    await tab(page, "returns").focus();
    await page.keyboard.press("F2");
    const input = page.getByTestId("table-rename-input");
    await expect(input).toBeFocused();
    await input.fill("Orders");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("table-rename")).toContainText(
      "Another data table has this name.",
    );
    await input.fill("refunds");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("table-rename")).toBeHidden();
    await expect(tab(page, "refunds")).toBeFocused();
    await expect(page.getByRole("tab")).toHaveText(["Sheet1", "orders", "refunds"]);
    await page.reload();
    await expect(page.getByRole("tab")).toHaveText(["Sheet1", "orders", "refunds"]);
  });

  test("have a menu, which moves tables", async ({ page }) => {
    await tab(page, "orders").focus();
    await page.keyboard.press("Shift+F10");
    const menu = page.getByTestId("table-tab-menu");
    await expect(menu).toBeVisible();
    await expect(menu.getByTestId("menu-tableMoveLeft")).toHaveAttribute("aria-disabled", "true");
    await menu.getByTestId("menu-tableMoveRight").click();
    await expect(page.getByRole("tab")).toHaveText(["Sheet1", "returns", "orders"]);
    await tab(page, "returns").click({ button: "right" });
    await expect(tab(page, "returns")).toHaveAttribute("aria-selected", "true");
    await menu.getByTestId("menu-tableMoveRight").click();
    await expect(page.getByRole("tab")).toHaveText(["Sheet1", "orders", "returns"]);
  });

  test("delete tables, once that is confirmed", async ({ page }) => {
    const before = await tableFiles(page);
    await tab(page, "orders").click();
    await tab(page, "orders").focus();
    await page.keyboard.press("Delete");
    const dialog = page.getByTestId("table-delete");
    await expect(dialog).toContainText("Delete orders and its 20 rows? This cannot be undone.");
    await page.keyboard.press("Escape");
    await expect(tab(page, "orders")).toBeFocused();
    await page.keyboard.press("Delete");
    await page.getByTestId("table-delete-confirm").click();
    await expect(page.getByRole("tab")).toHaveText(["Sheet1", "returns"]);
    await expect(tab(page, "Sheet1")).toHaveAttribute("aria-selected", "true");
    await expect.poll(async () => (await tableFiles(page)).length).toBe(before.length - 1);
  });

  test("export tables, which can be imported again", async ({ page }) => {
    await tab(page, "returns").click({ button: "right" });
    const download = page.waitForEvent("download");
    await page.getByTestId("menu-tableExport").click();
    const file = await download;
    expect(file.suggestedFilename()).toBe("returns.rtbl");
    page.once("filechooser", () => {});
    await runCommand(page, "importTable");
    await page.getByTestId("table-import-input").setInputFiles({
      name: "again.rtbl",
      mimeType: "application/octet-stream",
      buffer: await readFile(await file.path()),
    });
    await page.getByTestId("table-import-start").click();
    await expect(tab(page, "again")).toHaveAttribute("aria-selected", "true");
    await expect(page.getByTestId("table-count")).toHaveText("30 rows");
  });
});

test.describe("rows", () => {
  test.beforeEach(async ({ page }) => {
    await importTable(page, "orders", ordersCsv(1_000));
  });

  test("are sorted by a column, by more of them, and not at all", async ({ page }) => {
    await goTo(page, "region 1");
    await columnMenu(page, "tableSortAscending");
    await expectSaid(page, /Sorted by region, ascending/);
    await expect.poll(() => sortOfSelected(page)).toBe("ascending");
    // In lower case, so that the case does not matter.
    expect(await textsOf(page, "region", [1, 251, 501, 751])).toEqual([
      "East",
      "North",
      "south",
      "west",
    ]);
    // Rows that are equal stay in the order of the file.
    expect(await textsOf(page, "id", [1, 2])).toEqual(["2", "6"]);
    await goTo(page, "amount 1");
    await columnMenu(page, "tableSortAdd");
    await expectSaid(page, /Sorted by region, ascending; amount, ascending/);
    await columnMenu(page, "tableSortAdd");
    await expectSaid(page, /Sorted by region, ascending; amount, descending/);
    const amounts = (await textsOf(page, "amount", [1, 2, 250])).map(Number);
    expect([...amounts].sort((a, b) => b - a)).toEqual(amounts);
    expect(await textsOf(page, "region", [250, 251])).toEqual(["East", "North"]);
    await columnMenu(page, "tableSortClear");
    await expectSaid(page, /Not sorted any more/);
    expect(await textsOf(page, "id", [1, 2])).toEqual(["1", "2"]);
  });

  test("with blanks come last in both directions", async ({ page }) => {
    await goTo(page, "note 1");
    await columnMenu(page, "tableSortDescending");
    expect((await textsOf(page, "note", [1, 800, 801, 1_000])).map((text) => text !== "")).toEqual([
      true,
      true,
      false,
      false,
    ]);
  });

  test("are filtered by values, which can be searched for", async ({ page }) => {
    await goTo(page, "note 1");
    await columnMenu(page, "tableFilterValues");
    const dialog = page.getByTestId("table-filter");
    await expect(dialog.getByTestId("table-values-blank")).toBeChecked();
    await expect(dialog.locator(".table-values label")).toHaveCount(18);
    await expect(dialog.locator(".table-values label").first()).toContainText("200");
    await dialog.getByTestId("table-values-none").click();
    await expect(dialog.getByTestId("table-filter-apply")).toHaveAttribute("aria-disabled", "true");
    await dialog.getByTestId("table-values-search").fill("NOTE 1");
    await expect(dialog.locator(".table-values label")).toHaveCount(9);
    await dialog.getByLabel(/^note 10/).check();
    await dialog.getByLabel(/^note 11/).check();
    await dialog.getByTestId("table-filter-apply").click();
    await expect(dialog).toBeHidden();
    await expect(tableGrid(page)).toBeFocused();
    await expect(page.getByTestId("table-count")).toHaveText(/^\d+ of 1,000 rows$/);
    await expectSaid(page, /\d+ of 1,000 rows shown/);
    for (const text of await textsOf(page, "note", [1, 2, 3, 20])) {
      expect(text).toMatch(/^note 1[01]$/);
    }
    // What was chosen is so when it is opened again.
    await columnMenu(page, "tableFilterValues");
    await expect(dialog.getByLabel(/^note 10/)).toBeChecked();
    await expect(dialog.getByLabel(/^note 12/)).not.toBeChecked();
    await dialog.getByTestId("table-filter-clear").click();
    await expect(page.getByTestId("table-count")).toHaveText("1,000 rows");
  });

  test("stay filtered by a column when what it has is applied as it is", async ({ page }) => {
    const dialog = page.getByTestId("table-filter");
    const choose = async (column: string, value: RegExp) => {
      await goTo(page, `${column} 1`);
      await columnMenu(page, "tableFilterValues");
      await dialog.getByTestId("table-values-none").click();
      await dialog.getByLabel(value).check();
      await dialog.getByTestId("table-filter-apply").click();
      await expect(dialog).toBeHidden();
    };
    await importTable(
      page,
      "cities",
      "region,city\nNorth,Oslo\nNorth,Bergen\nSouth,Rome\nSouth,Oslo\nEast,Seoul\n",
    );
    await choose("region", /^North/);
    await choose("city", /^Bergen/);
    await expect(page.getByTestId("table-count")).toHaveText("1 of 5 rows");
    // The only region there is now is one that was chosen, which does not make it all of them.
    await goTo(page, "region 1");
    await columnMenu(page, "tableFilterValues");
    await expect(dialog.locator(".table-values label")).toHaveText([/^North/]);
    await dialog.getByTestId("table-filter-apply").click();
    await expect(dialog).toBeHidden();
    await runCommand(page, "tablePanel");
    await expect(page.getByTestId("table-filters").getByRole("listitem")).toHaveCount(2);
    await goTo(page, "city 1");
    await columnMenu(page, "tableFilterClear");
    await expect(page.getByTestId("table-count")).toHaveText("2 of 5 rows");
    // All of those that there are is no condition.
    await goTo(page, "region 1");
    await columnMenu(page, "tableFilterValues");
    await dialog.getByTestId("table-values-all").click();
    await dialog.getByTestId("table-filter-apply").click();
    await expect(page.getByTestId("table-count")).toHaveText("5 rows");
    await expect(page.getByTestId("table-filters")).toBeHidden();
  });

  test("are filtered by what a search has found", async ({ page }) => {
    const dialog = page.getByTestId("table-filter");
    await goTo(page, "note 1");
    await columnMenu(page, "tableFilterValues");
    await expect(dialog.locator(".table-values label")).toHaveCount(18);
    await dialog.getByTestId("table-values-search").fill("note 1");
    await expect(dialog.locator(".table-values label")).toHaveCount(9);
    await expect(dialog.getByLabel(/^note 12/)).toBeChecked();
    await page.keyboard.press("Enter");
    await expect(dialog).toBeHidden();
    await expect(page.getByTestId("table-count")).toHaveText(/^3\d\d of 1,000 rows$/);
    for (const text of await textsOf(page, "note", [1, 2, 3, 20])) {
      expect(text).toMatch(/^note 1\d?$/);
    }
  });

  test("with long texts are listed within the dialog", async ({ page }) => {
    const long = (text: string) => `${text} `.repeat(80);
    await importTable(page, "long", `text,n\n${long("first")},1\n${long("second")},2\n`);
    await goTo(page, "text 1");
    await columnMenu(page, "tableFilterValues");
    const dialog = page.getByTestId("table-filter");
    await expect(dialog.locator(".table-values label")).toHaveCount(2);
    const { over, wide } = await dialog.evaluate((element) => ({
      over: Math.max(
        ...[element, ...element.querySelectorAll("fieldset")].map(
          ({ scrollWidth, clientWidth }) => scrollWidth - clientWidth,
        ),
      ),
      wide: element.getBoundingClientRect().width,
    }));
    expect(over).toBe(0);
    expect(wide).toBeLessThanOrEqual(760);
    await expect(dialog.locator(".table-values .table-count").first()).toBeInViewport();
  });

  test("are filtered by the value that is shown, whatever was typed before", async ({ page }) => {
    await importTable(
      page,
      "times",
      "at,n\n2024-01-01T10:00:00,1.5\n2024-01-02T11:30:00,2.5\n2024-01-03T00:00:00,3\n",
    );
    await goTo(page, "at 1");
    await columnMenu(page, "tableFilterCondition");
    const choice = page.getByTestId("table-condition");
    const value = page.getByTestId("table-value-a");
    await choice.selectOption("time:on");
    await value.fill("2024-01-02");
    await choice.selectOption("time:before");
    await expect(value).toHaveValue("2024-01-02T00:00");
    await choice.selectOption("largest");
    await expect(value).toHaveValue("");
    await expect(page.getByTestId("table-filter-apply")).toHaveAttribute("aria-disabled", "true");
    await value.fill("2");
    await choice.selectOption("time:after");
    await expect(value).toHaveValue("");
    await expect(page.getByTestId("table-filter-apply")).toHaveAttribute("aria-disabled", "true");
    await value.fill("2024-01-02T11:30");
    await choice.selectOption("time:on");
    await expect(value).toHaveValue("2024-01-02");
    await page.getByTestId("table-filter-apply").click();
    await expect(page.getByTestId("table-count")).toHaveText("1 of 3 rows");
  });

  test("are filtered by conditions, as the columns have them", async ({ page }) => {
    const dialog = page.getByTestId("table-filter");
    const filter = async (column: string, choice: string, a?: string, b?: string) => {
      await goTo(page, `${column} 1`);
      await columnMenu(page, "tableFilterCondition");
      await dialog.getByTestId("table-condition").selectOption(choice);
      if (a !== undefined) {
        await dialog.getByTestId("table-value-a").fill(a);
      }
      if (b !== undefined) {
        await dialog.getByTestId("table-value-b").fill(b);
      }
      await dialog.getByTestId("table-filter-apply").click();
      await expect(dialog).toBeHidden();
    };
    await filter("id", "number:between", "20", "11");
    await expect(page.getByTestId("table-count")).toHaveText("10 of 1,000 rows");
    await filter("note", "notBlank");
    await expect(page.getByTestId("table-count")).toHaveText("8 of 1,000 rows");
    await filter("region", "startsWith", "NO");
    await expect(page.getByTestId("table-count")).toHaveText(/^[12] of 1,000 rows$/);
    await runCommand(page, "clearFilters");
    await expect(page.getByTestId("table-count")).toHaveText("1,000 rows");
    await filter("amount", "largest", "3");
    await expect(page.getByTestId("table-count")).toHaveText("3 of 1,000 rows");
    await goTo(page, "amount 1");
    await columnMenu(page, "tableFilterClear");
    await filter("day", "time:on", "2024-02-01");
    await expect(page.getByTestId("table-count")).toHaveText("3 of 1,000 rows");
    expect(await textsOf(page, "day", [1, 3])).toEqual(["2024-02-01", "2024-02-01"]);
  });

  test("are not filtered by what is no number, and why is said", async ({ page }) => {
    await goTo(page, "amount 1");
    await columnMenu(page, "tableFilterCondition");
    const dialog = page.getByTestId("table-filter");
    await dialog.getByTestId("table-value-a").fill("much");
    await dialog.getByTestId("table-filter-apply").click();
    await expect(dialog.getByTestId("table-filter-problem")).toHaveText(
      "The condition does not fit the column.",
    );
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("table-count")).toHaveText("1,000 rows");
    await expect(page.getByTestId("table-refused")).toBeHidden();
  });

  test("that none is left of can be had back", async ({ page }) => {
    await goTo(page, "id 1");
    await columnMenu(page, "tableFilterCondition");
    await page.getByTestId("table-value-a").fill("99999");
    await page.getByTestId("table-filter-apply").click();
    await expect(page.getByTestId("table-count")).toHaveText("0 of 1,000 rows");
    // There is no cell then, and still a column.
    await columnMenu(page, "tableFilterClear");
    await expect(page.getByTestId("table-count")).toHaveText("1,000 rows");
    await expect.poll(() => selectedCell(page)).toMatchObject({ row: "1", text: "1" });
  });

  test("stay sorted and filtered after a reload, and where they were left", async ({ page }) => {
    await goTo(page, "amount 1");
    await columnMenu(page, "tableSortDescending");
    await goTo(page, "region 1");
    await columnMenu(page, "tableFilterValues");
    await page.getByTestId("table-values-none").click();
    await page.getByTestId("table-filter").getByLabel(/^East/).check();
    await page.getByTestId("table-filter-apply").click();
    await expect(page.getByTestId("table-count")).toHaveText("250 of 1,000 rows");
    await goTo(page, "amount 200");
    const before = await filledCell(page);
    await page.reload();
    await expect(page.getByTestId("table-count")).toHaveText("250 of 1,000 rows");
    await expect.poll(() => selectedCell(page)).toEqual(before);
    await expect.poll(() => sortOfSelected(page)).toBe("descending");
  });
});

test.describe("the panel", () => {
  test.beforeEach(async ({ page }) => {
    await importTable(page, "orders", ordersCsv(1_000));
    await runCommand(page, "tablePanel");
    await expect(page.getByTestId("table-panel")).toBeVisible();
  });

  const figure = (page: Page, column: number, name: string) =>
    page.getByTestId(`table-column-${column}`).locator(`[data-figure="${name}"]`);

  test("has the totals of the rows that are shown", async ({ page }) => {
    await expect(figure(page, 0, "sum")).toHaveText("500500");
    await expect(figure(page, 0, "min")).toHaveText("1");
    await expect(figure(page, 0, "max")).toHaveText("1000");
    await expect(figure(page, 1, "distinct")).toHaveText("4");
    await expect(figure(page, 3, "min")).toHaveText("2024-01-01");
    await expect(figure(page, 4, "blanks")).toHaveText("200");
    await goTo(page, "id 1");
    await columnMenu(page, "tableFilterCondition");
    await page.getByTestId("table-condition").selectOption("number:atMost");
    await page.getByTestId("table-value-a").fill("10");
    await page.getByTestId("table-filter-apply").click();
    await expect(page.getByTestId("table-rows")).toHaveText("10 of 1,000 rows shown");
    await expect(figure(page, 0, "sum")).toHaveText("55");
    await expect(figure(page, 4, "blanks")).toHaveText("2");
  });

  test("takes away what the rows are sorted and filtered by, one by one", async ({ page }) => {
    await goTo(page, "amount 1");
    await columnMenu(page, "tableSortDescending");
    await goTo(page, "region 1");
    await columnMenu(page, "tableSortAdd");
    await columnMenu(page, "tableFilterCondition");
    await page.getByTestId("table-value-a").fill("st");
    await page.getByTestId("table-filter-apply").click();
    await expect(page.getByTestId("table-filters")).toHaveText(/region: contains “st”/);
    await expect(page.getByTestId("table-order").getByRole("listitem")).toHaveText([
      /amount, descending/,
      /region, ascending/,
    ]);
    await page.getByRole("button", { name: "Do not sort by amount" }).click();
    await expect(page.getByTestId("table-order").getByRole("listitem")).toHaveText([
      /region, ascending/,
    ]);
    await page.getByRole("button", { name: "Do not filter by region" }).click();
    await expect(page.getByTestId("table-filters")).toBeHidden();
    await expect(page.getByTestId("table-rows")).toHaveText("1,000 of 1,000 rows shown");
  });

  test("takes away two things at once", async ({ page }) => {
    await goTo(page, "amount 1");
    await columnMenu(page, "tableSortDescending");
    await goTo(page, "region 1");
    await columnMenu(page, "tableSortAdd");
    await expect(page.getByTestId("table-order").getByRole("listitem")).toHaveCount(2);
    // The second is clicked before anything has come of the first.
    await page.evaluate(() => {
      for (const key of ["sort-2", "sort-1"]) {
        document.querySelector<HTMLElement>(`[data-testid="table-remove-${key}"]`)?.click();
      }
    });
    await expect(page.getByTestId("table-order")).toBeHidden();
    expect(await textsOf(page, "id", [1, 2])).toEqual(["1", "2"]);
  });

  test("goes back to what is shown when a question is refused", async ({ page }) => {
    await goTo(page, "amount 1");
    await columnMenu(page, "tableFilterCondition");
    await page.getByTestId("table-value-a").fill("much");
    await page.getByTestId("table-filter-apply").click();
    await expect(page.getByTestId("table-filter-problem")).toBeVisible();
    await page.keyboard.press("Escape");
    await columnMenu(page, "tableSortDescending");
    await expect(page.getByTestId("table-order").getByRole("listitem")).toHaveText([
      /amount, descending/,
    ]);
    await expect(page.getByTestId("table-filters")).toBeHidden();
  });

  test("shows the rows there are when a question is given up whose answer was on its way", async ({
    page,
  }) => {
    const filter = async (column: string, choice: string, value: string) => {
      await goTo(page, `${column} 1`);
      await columnMenu(page, "tableFilterCondition");
      await page.getByTestId("table-condition").selectOption(choice);
      await page.getByTestId("table-value-a").fill(value);
      await page.getByTestId("table-filter-apply").click();
      await expect(page.getByTestId("table-filter")).toBeHidden();
    };
    await filter("region", "contains", "north");
    await filter("amount", "number:less", "100");
    await expect(page.getByTestId("table-filters").getByRole("listitem")).toHaveCount(2);
    // The page takes its time, so that the answer to the first question has come by then.
    const isCancelled = await page.evaluate(async () => {
      const part = (id: string) => document.querySelector<HTMLElement>(`[data-testid="${id}"]`);
      const settle = async () => {
        for (let turn = 0; turn < 20; turn++) {
          await null;
        }
      };
      part("table-remove-filter-1")?.click();
      await settle();
      const until = performance.now() + 1_000;
      while (performance.now() < until) {
        // Nothing else is done meanwhile.
      }
      part("table-remove-filter-2")?.click();
      await settle();
      const cancel = part("table-job-cancel");
      cancel?.click();
      return cancel !== null;
    });
    expect(isCancelled).toBe(true);
    await expect(page.getByTestId("table-job")).toBeHidden();
    // Whichever answer came last, the rows are those that are told of.
    const asked = (await page.getByTestId("table-filters").allTextContents()).join();
    const isShown = (row: number) =>
      (!asked.includes("region") || row % 4 === 0) &&
      (!asked.includes("amount") || (row * 7_919) % 100_000 < 10_000);
    const shown = Array.from({ length: 1_000 }, (_, at) => at + 1).filter(isShown);
    await expect(page.getByTestId("table-count")).toHaveText(
      shown.length === 1_000 ? "1,000 rows" : `${shown.length} of 1,000 rows`,
    );
    const places = [1, 2, 20, shown.length];
    expect(await textsOf(page, "id", places)).toEqual(
      places.map((place) => String(shown[place - 1])),
    );
  });

  test("is a part of the window that the keys go to, and leave", async ({ page }) => {
    const panel = page.getByTestId("table-panel");
    await expect(panel.getByRole("button", { name: "Close" })).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(tableGrid(page)).toBeFocused();
    await page.keyboard.press("Shift+F6");
    await page.keyboard.press("F6");
    await expect(tableGrid(page)).toBeFocused();
    for (let presses = 0; presses < 8; presses++) {
      await page.keyboard.press("F6");
      if (await panel.evaluate((element) => element.contains(document.activeElement))) {
        break;
      }
    }
    await expect
      .poll(() => panel.evaluate((element) => element.contains(document.activeElement)))
      .toBe(true);
    await panel.getByRole("button", { name: "amount" }).press("Enter");
    await expect(tableGrid(page)).toBeFocused();
    await expect(page.getByTestId("name-box")).toHaveText("amount 1:amount 1000");
    await expect(page.getByTestId("table-total-2")).toContainText("sum");
    await panel.getByRole("button", { name: "Close" }).click();
    await expect(panel).toBeHidden();
  });
});

test.describe("cells", () => {
  test("are copied as text and as a table, and pasted into a worksheet", async ({
    page,
    context,
    browserName,
  }) => {
    await importTable(page, "orders", ordersCsv(100));
    if (browserName === "chromium") {
      await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    }
    await goTo(page, "id 1");
    await page.keyboard.press("Shift+ArrowRight");
    await page.keyboard.press("Shift+ArrowRight");
    await page.keyboard.press("Shift+ArrowDown");
    await page.keyboard.press("ControlOrMeta+c");
    await expectSaid(page, /6 cells copied/);
    if (browserName === "chromium") {
      const copied = await page.evaluate(() => navigator.clipboard.readText());
      expect(copied.replaceAll("\r\n", "\n")).toBe("1\tsouth\t79.19\n2\tEast\t158.38");
    }
    await tab(page, "Sheet1").click();
    await page.keyboard.press("ControlOrMeta+v");
    await expect
      .poll(() =>
        page.evaluate(() => [
          window.__reticula?.textAt(0, 0),
          window.__reticula?.textAt(0, 1),
          window.__reticula?.textAt(1, 2),
        ]),
      )
      .toEqual(["1", "south", "158.38"]);
  });

  test("that are texts stay texts in a worksheet", async ({ page, context, browserName }) => {
    const texts = ["00123", "12345678901234567890123", "=1+1", "'quoted", "1-2", "TRUE", "a<b>&c"];
    await importTable(
      page,
      "texts",
      `text,n,empty\n${texts.map((text, at) => `"${text}",${at + 1},`).join("\n")}\nplain,-8,\n`,
    );
    if (browserName === "chromium") {
      await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    }
    await page.keyboard.press("ControlOrMeta+a");
    await page.keyboard.press("ControlOrMeta+c");
    await expectSaid(page, /24 cells copied/);
    await tab(page, "Sheet1").click();
    await page.keyboard.press("ControlOrMeta+v");
    const pasted = (col: number) =>
      page.evaluate(
        ([at, rows]) =>
          Array.from({ length: rows ?? 0 }, (_, row) => window.__reticula?.textAt(row, at ?? 0)),
        [col, texts.length + 1],
      );
    await expect.poll(() => pasted(1)).toEqual(["1", "2", "3", "4", "5", "6", "7", "-8"]);
    expect(await pasted(0)).toEqual([...texts, "plain"]);
    expect(await pasted(2)).toEqual(Array(8).fill(""));
    // Numbers are numbers.
    await page.evaluate(() => window.__reticula?.select(9, 1));
    await page.keyboard.type("=SUM(B1:B8)+COUNTA(A1:C8)");
    await page.keyboard.press("Enter");
    await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(9, 1))).toBe("36");
  });

  test("that are copied leave nothing of those there were too many of", async ({ page }) => {
    await importTable(page, "orders", ordersCsv(50_000));
    await page.keyboard.press("ControlOrMeta+a");
    await page.keyboard.press("ControlOrMeta+c");
    await expect(page.getByTestId("table-refused")).toBeVisible();
    await goTo(page, "id 3");
    await page.keyboard.press("ControlOrMeta+c");
    await expect(page.getByTestId("table-refused")).toBeHidden();
  });

  test("are not copied when there are too many", async ({ page }) => {
    await importTable(page, "orders", ordersCsv(50_000));
    await page.keyboard.press("ControlOrMeta+a");
    await page.keyboard.press("ControlOrMeta+c");
    await expect(page.getByTestId("table-refused")).toContainText(
      "No more than 200,000 cells can be copied at once.",
    );
    await page.getByTestId("table-refused").getByRole("button", { name: "Dismiss" }).click();
    await expect(page.getByTestId("table-refused")).toBeHidden();
    await expect(tableGrid(page)).toBeFocused();
  });
});

test.describe("the look of a table", () => {
  test.beforeEach(async ({ page }) => {
    await importTable(page, "orders", ordersCsv(1_000));
  });

  test("has the zoom of the status bar, which each tab has its own of", async ({ page }) => {
    await page.getByTestId("zoom-in").click();
    await expect(page.getByTestId("zoom-value")).toHaveText("110%");
    await runCommand(page, "zoom:200");
    await expect(page.getByTestId("zoom-value")).toHaveText("200%");
    await tab(page, "Sheet1").click();
    await expect(page.getByTestId("zoom-value")).toHaveText("100%");
    await tab(page, "orders").click();
    await expect(page.getByTestId("zoom-value")).toHaveText("200%");
    await page.reload();
    await expect(view(page)).toHaveAttribute("data-status", "open");
    await expect(page.getByTestId("zoom-value")).toHaveText("200%");
  });

  test("has a zoom that is told of when it comes into view", async ({ page }) => {
    await tab(page, "Sheet1").click();
    await runCommand(page, "zoom:150");
    await expect(page.getByTestId("zoom-value")).toHaveText("150%");
    await tab(page, "orders").click();
    await expect(page.getByTestId("zoom-value")).toHaveText("100%");
    await page.getByTestId("zoom-in").click();
    await expect(page.getByTestId("zoom-value")).toHaveText("110%");
    await tab(page, "Sheet1").click();
    await expect(page.getByTestId("zoom-value")).toHaveText("150%");
  });

  test("is forgotten with the table", async ({ page }) => {
    const stored = () => page.evaluate(() => localStorage.getItem("reticula:table-view:v1") ?? "");
    await expect.poll(stored).toContain("frozenCols");
    await runCommand(page, "tableDelete");
    await page.getByTestId("table-delete-confirm").click();
    await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
    await page.waitForTimeout(800);
    expect(await stored()).not.toContain("frozenCols");
  });

  test("has frozen columns and widths, which stay", async ({ page }) => {
    await goTo(page, "amount 3");
    await runCommand(page, "freeze:active");
    await expectSaid(page, /Columns frozen: 2/);
    await runCommand(page, "colWidth");
    await page.getByTestId("size-input").fill("300");
    await page.getByTestId("size-apply").click();
    await expect(tableGrid(page)).toBeFocused();
    await page.waitForTimeout(700);
    const stored = () =>
      page.evaluate(() => {
        const { documents } = JSON.parse(localStorage.getItem("reticula:table-view:v1") ?? "{}");
        const [look] = Object.values(documents?.[0]?.tables ?? {}) as {
          widths: unknown;
          frozenCols: unknown;
        }[];
        return { widths: look?.widths, frozenCols: look?.frozenCols };
      });
    expect(await stored()).toEqual({ widths: [[2, 300]], frozenCols: 2 });
    await page.reload();
    await expect(view(page)).toHaveAttribute("data-status", "open");
    await runCommand(page, "colWidth");
    await expect(page.getByTestId("size-input")).toHaveValue("300");
    await page.keyboard.press("Escape");
    await runCommand(page, "freeze:none");
    await expectSaid(page, /No columns frozen/);
  });
});

test.describe("what shows tables", () => {
  // Those loaded by a service worker could not be kept from the page.
  test.use({ serviceWorkers: "block" });

  test("can be had again when it could not be loaded, and the worksheets go on", async ({
    page,
  }) => {
    await importTable(page, "orders", ordersCsv(100));
    await tab(page, "Sheet1").click();
    await page.waitForTimeout(700);
    await page.route(/\/assets\/tables-/, (route) => route.abort());
    await openWorkbook(page);
    await tab(page, "orders").click();
    const missing = page.getByTestId("tables-missing");
    await expect(missing).toContainText("could not be loaded");
    await expect(page.getByRole("tab")).toHaveText(["Sheet1", "orders"]);
    await tab(page, "Sheet1").click();
    await expect(missing).toBeHidden();
    await page.keyboard.type("goes on");
    await page.keyboard.press("Enter");
    await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("goes on");
    // What is asked for is refused, and why is said.
    await runCommand(page, "importTable");
    await expect(page.getByTestId("rejected-banner")).toContainText("could not be loaded");
    await tab(page, "orders").click();
    await expect(missing).toBeVisible();
    await page.unroute(/\/assets\/tables-/);
    await missing.getByRole("button", { name: "Try Again" }).click();
    await expect(view(page)).toHaveAttribute("data-status", "open");
    await expect(missing).toBeHidden();
  });
});
