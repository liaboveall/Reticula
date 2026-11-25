import { expect, type Locator, type Page, test } from "@playwright/test";
import { settled } from "./collab-helpers.ts";
import { expectFocusInside, pressTab, tabTo } from "./keyboard-helpers.ts";

// What is read out of the grid, which is a picture: the live region's texts, the
// selected cell as a table, and how the parts of the grid are called.

declare global {
  interface Window {
    /** Everything that has been put into the page's live region, in order. */
    __said?: string[];
  }
}

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const said: string[] = [];
    window.__said = said;
    let shown = "";
    new MutationObserver(() => {
      const text = document.querySelector('[data-testid="page-status"]')?.textContent ?? "";
      if (text !== shown) {
        shown = text;
        if (text !== "") {
          said.push(text);
        }
      }
    }).observe(document, { childList: true, subtree: true, characterData: true });
  });
});

/** Waits until something has been read, forgetting it and what came before. */
async function expectSaid(page: Page, expected: string | RegExp): Promise<void> {
  await expect
    .poll(
      () =>
        page.evaluate(
          ([text, isPattern]) => {
            const said = window.__said ?? [];
            const matches = (one: string) =>
              isPattern ? new RegExp(text).test(one) : one === text;
            const at = said.findIndex(matches);
            if (at >= 0) {
              said.splice(0, at + 1);
              return text;
            }
            return said;
          },
          typeof expected === "string"
            ? ([expected, false] as const)
            : ([expected.source, true] as const),
        ),
      { message: `"${expected}" to be read` },
    )
    .toEqual(typeof expected === "string" ? expected : expected.source);
}

/**
 * The same for what has come of an action and the cell that is selected then.
 * They are one text, unless the engine takes its time to say what is in the cell.
 */
async function expectSaidWithPlace(page: Page, event: string, place: string): Promise<void> {
  await expect
    .poll(
      () =>
        page.evaluate(
          ([event, place]) => {
            const said = window.__said ?? [];
            const joined = [`${event}. ${place}`, `${event}。${place}`];
            for (const [at, text] of said.entries()) {
              const count = joined.includes(text)
                ? 1
                : text === event && said[at + 1] === place
                  ? 2
                  : 0;
              if (count > 0) {
                said.splice(0, at + count);
                return place;
              }
            }
            return said;
          },
          [event, place] as const,
        ),
      { message: `"${event}" and "${place}" to be read` },
    )
    .toEqual(place);
}

/** Forgets what has been read so far, once nothing is waiting to be read. */
async function silence(page: Page): Promise<void> {
  await page.waitForTimeout(400);
  await page.evaluate(() => window.__said?.splice(0));
}

async function said(page: Page): Promise<string[]> {
  return page.evaluate(() => window.__said ?? []);
}

const textAt = (page: Page, row: number, col: number) =>
  page.evaluate(([r, c]) => window.__reticula?.textAt(r, c), [row, col] as const);

const select = (page: Page, row: number, col: number) =>
  page.evaluate(([r, c]) => window.__reticula?.select(r, c), [row, col] as const);

/** Opens a workbook of its own, with the keyboard in the grid, which says where that is. */
async function open(page: Page, doc: string, lang = "en"): Promise<void> {
  const { name } = test.info().project;
  await page.goto(`/app?hooks&lang=${lang}&doc=${doc}-${name}`);
  await expect(page.getByTestId("engine-status")).toHaveAttribute("data-status", "ready", {
    timeout: 20_000,
  });
  await settled(page);
  await page.getByTestId("grid").focus();
  await expectFocusInside(page.getByTestId("grid"));
  await expectSaid(page, /^A1\b/);
}

async function type(page: Page, text: string): Promise<void> {
  await page.keyboard.type(text);
  await page.keyboard.press("Enter");
}

/** The middle of a row's or a column's header, when all sizes are the standard ones. */
async function header(page: Page, axis: "row" | "col", index: number) {
  const box = await page.getByTestId("grid").boundingBox();
  const metrics = await page.evaluate(() => window.__reticula?.stats().metrics);
  if (!box || !metrics) {
    throw new Error("the grid is not there");
  }
  const { headerWidth, headerHeight, rowHeight, colWidth } = metrics;
  return axis === "row"
    ? { x: box.x + headerWidth / 2, y: box.y + headerHeight + rowHeight * (index + 0.5) }
    : { x: box.x + headerWidth + colWidth * (index + 0.5), y: box.y + headerHeight / 2 };
}

test("moving reads the cell, what it shows and where that comes from", async ({ page }) => {
  await open(page, "announce-move");
  await type(page, "1234.5");
  await expectSaidWithPlace(page, "Entered 1234.5 in A1", "A2, blank");
  await type(page, "=A1*2");
  await expectSaidWithPlace(page, "Entered =A1*2 in A2", "A3, blank");
  await page.keyboard.press("ArrowUp");
  await expectSaid(page, "A2, 2469, formula =A1*2");
  await page.keyboard.press("ArrowUp");
  await expectSaid(page, "A1, 1234.5");
  // At the edge there is nothing new to say.
  await silence(page);
  await page.keyboard.press("ArrowUp");
  await page.waitForTimeout(300);
  expect(await said(page)).toEqual([]);

  // An error, and a formula that spills.
  await select(page, 2, 0);
  await type(page, "=A1/0");
  await page.keyboard.press("ArrowUp");
  await expectSaid(page, /A3, error #DIV\/0!, formula =A1\/0$/);
  await select(page, 0, 2);
  await type(page, "=SEQUENCE(2,2)");
  await page.keyboard.press("ArrowRight");
  await expectSaid(page, /D2, 4, spilled from C1, formula =SEQUENCE\(2,2\)$/);
  await select(page, 0, 2);
  await expectSaid(page, "C1, 1, formula =SEQUENCE(2,2)");
  await select(page, 4, 4);
  await type(page, '=""');
  await page.keyboard.press("ArrowUp");
  await expectSaid(page, /E5, empty text, formula =""$/);

  // Whole texts are for reading in the formula bar.
  await select(page, 6, 0);
  await type(page, "x".repeat(300));
  await page.keyboard.press("ArrowUp");
  await expectSaid(page, /A7, x{200}, and 100 more characters$/);
  await page.keyboard.press("Delete");
  await expectSaid(page, "A7, blank");
  await page.keyboard.press("Shift+ArrowUp");
  await expectSaid(page, "A6 to A7 selected, 2 cells");
  await page.keyboard.press("Delete");
  await expectSaid(page, "Cleared A6 to A7");
});

test("ranges are read by where they begin and end", async ({ page }) => {
  await open(page, "announce-range");
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Shift+ArrowRight");
  await expectSaid(page, "A1 to B3 selected, 6 cells");
  await page.keyboard.press("ArrowDown");
  await expectSaid(page, "A2, blank");

  const row = await header(page, "row", 3);
  await page.mouse.click(row.x, row.y);
  await expectSaid(page, "Row 4 selected");
  // More rows, and not the cells at their far end.
  await page.keyboard.press("Shift+ArrowDown");
  await expectSaid(page, "Rows 4 to 5 selected, 2 rows");
  expect(await page.evaluate(() => window.__reticula?.stats().scroll.left)).toBe(0);
  const col = await header(page, "col", 1);
  await page.mouse.click(col.x, col.y);
  await expectSaid(page, "Column B selected");
  const more = await header(page, "col", 3);
  await page.keyboard.down("ControlOrMeta");
  await page.mouse.click(more.x, more.y);
  await page.keyboard.up("ControlOrMeta");
  await expectSaid(page, "Column D selected, 2 ranges selected");
  await page.keyboard.press("ControlOrMeta+a");
  await expectSaid(page, "All cells selected");
});

test("undoing and redoing say what, and then how the cell is", async ({ page }) => {
  await open(page, "announce-undo");
  await type(page, "5");
  await page.keyboard.press("ArrowUp");
  await expectSaid(page, /A1, 5$/);
  await silence(page);
  await page.keyboard.press("ControlOrMeta+z");
  await expectSaidWithPlace(page, "Undid: Edit A1", "A1, blank");
  await page.keyboard.press("ControlOrMeta+y");
  await expectSaidWithPlace(page, "Redid: Edit A1", "A1, 5");
  // Nothing is left to redo.
  await silence(page);
  await page.keyboard.press("ControlOrMeta+y");
  await page.waitForTimeout(300);
  expect(await said(page)).toEqual([]);
});

test("another sheet is read by its name, and the cell that was selected there", async ({
  page,
}) => {
  await open(page, "announce-sheets");
  await select(page, 1, 1);
  await type(page, "here");
  await page.keyboard.press("ArrowUp");
  await expectSaid(page, /B2, here$/);
  await page.getByTestId("add-sheet-btn").click();
  await expectSaidWithPlace(page, "added sheet. Worksheet Sheet2", "A1, blank");
  await page.getByTestId("sheet-tab-0").click();
  await expectSaidWithPlace(page, "Worksheet Sheet1", "B2, here");
});

test("under a filter, rows have the numbers they have in the sheet", async ({ page }) => {
  await open(page, "announce-filter");
  for (const value of ["Amount", "10", "20", "30", "40"]) {
    await type(page, value);
  }
  await expect.poll(() => textAt(page, 4, 0)).toBe("40");
  await page.evaluate(() => window.__reticula?.selectRange({ row: 0, col: 0 }, { row: 4, col: 0 }));
  await page.getByTestId("m3a-filter-btn").click();
  await expectSaid(page, /^Filter on/);
  const box = await page.getByTestId("grid").boundingBox();
  const metrics = await page.evaluate(() => window.__reticula?.stats().metrics);
  if (!box || !metrics) {
    throw new Error("the grid is not there");
  }
  await page.mouse.click(
    box.x + metrics.headerWidth + metrics.colWidth - 10,
    box.y + metrics.headerHeight + metrics.rowHeight * 0.5,
  );
  const dialog = page.getByTestId("m3a-filter-modal");
  await dialog.getByRole("button", { name: "Number" }).click();
  await page.getByTestId("filter-num-val").fill("20");
  await page.getByTestId("filter-apply-btn").click();
  await expect(dialog).toBeHidden();
  await expectSaid(page, /^[\d,]+ of [\d,]+ rows shown/);
  await expect.poll(() => textAt(page, 1, 0)).toBe("30");

  await page.getByTestId("grid").focus();
  await select(page, 0, 0);
  await page.keyboard.press("ArrowDown");
  await expectSaid(page, "A4, 30");
  await page.keyboard.press("Shift+ArrowDown");
  await expectSaid(page, "A4 to A5 selected, 2 cells");
  // The name box says the same, and goes where it says.
  await expect(page.getByTestId("name-box")).toHaveText("A4:A5");
  // The table says the same.
  await page.keyboard.press("ArrowDown");
  const table = page.getByRole("table", { name: "Selected cell" });
  await expect(table.locator("tbody th")).toHaveText("5");
  await expect(table.locator("tbody td")).toHaveText("40");
  await expect(table.locator("tbody tr")).toHaveAttribute("aria-rowindex", "6");

  await page.getByTestId("m3a-filter-btn").click();
  await expectSaid(page, /^Filter off, all [\d,]+ rows shown/);
});

test("a dialog says itself what comes of it, and not the cell behind it", async ({ page }) => {
  await open(page, "announce-dialog");
  await type(page, "needle");
  await expectSaidWithPlace(page, "Entered needle in A1", "A2, blank");
  await page.keyboard.press("ControlOrMeta+f");
  await expect(page.getByTestId("find-query")).toBeFocused();
  await silence(page);
  await page.keyboard.type("needle");
  await page.getByTestId("find-next-btn").click();
  await expect(page.locator('dialog:modal > [role="status"]')).toHaveText("Jumped to Sheet1!A1");
  expect(await said(page)).toEqual([]);
  // Back in the grid, the cell is read.
  await page.keyboard.press("Escape");
  await expectFocusInside(page.getByTestId("grid"));
  await expectSaid(page, "A1, needle");
});

test("whoever is in a drawer hears of it, and not of the cells it changes", async ({ page }) => {
  await open(page, "announce-drawer");
  await page.getByTestId("ai-panel-btn").click();
  const panel = page.getByRole("complementary", { name: "AI Analyst" });
  await expect(panel.getByRole("heading", { name: "AI Analyst" })).toBeFocused();
  await silence(page);
  await page.getByTestId("ai-demo-regional-sales-summary").focus();
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("ai-draft-banner")).toBeVisible();
  await page.waitForTimeout(400);
  expect((await said(page)).filter((text) => /A1|Worksheet/.test(text))).toEqual([]);
  // Back in the grid, the cell is read.
  await page.getByTestId("grid").focus();
  await expectSaid(page, /^A1, /);
});

test("recalculation says nothing of itself", async ({ page }) => {
  await page.goto("/app?hooks&lang=en");
  await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
  await settled(page);
  await page.getByTestId("grid").focus();
  await select(page, 0, 21);
  await expectSaid(page, /^V1, /);
  await silence(page);
  // Some hundred thousand formulas depend on it.
  await type(page, "0.05");
  await expect(page.getByTestId("recalc-status")).toContainText("recalculated", {
    timeout: 30_000,
  });
  await page.waitForTimeout(400);
  const read = (await said(page)).join(" ");
  expect(read).toMatch(/^Entered 0\.05 in V1\.? V2, /);
  expect(read).not.toMatch(/recalculat/);
});

test("in Chinese", async ({ page }) => {
  await open(page, "announce-zh", "zh");
  await type(page, "=1/0");
  await expectSaidWithPlace(page, "已在 A1 输入 =1/0", "A2，空");
  await page.keyboard.press("ArrowUp");
  await expectSaid(page, "A1，错误 #DIV/0!，公式 =1/0");
  await page.keyboard.press("Shift+ArrowDown");
  await expectSaid(page, "已选中 A1 到 A2，共 2 个单元格");
  await page.keyboard.press("ControlOrMeta+z");
  await expectSaid(page, /^已撤销：编辑 A1/);
  await page.getByTestId("add-sheet-btn").click();
  await expectSaidWithPlace(page, "已添加工作表。工作表 Sheet2", "A1，空");
  await expect(page.getByRole("textbox", { name: "单元格编辑器" })).toBeAttached();
  await expect(page.getByRole("table", { name: "选中的单元格" })).toBeAttached();
});

test("the grid has a table with the selected cell, and one named input", async ({ page }) => {
  await open(page, "announce-table");
  const grid = page.getByRole("group", { name: "Worksheet grid" });
  await expect(grid).toHaveAttribute("data-testid", "grid");
  const table = grid.getByRole("table", { name: "Selected cell" });
  const { rows, cols } = await page.evaluate(() => {
    const { sheet } = window.__reticula?.stats() ?? { sheet: { rows: 0, cols: 0 } };
    return sheet;
  });
  // The headers count as a row and as a column.
  await expect(table).toHaveAttribute("aria-rowcount", String(rows + 1));
  await expect(table).toHaveAttribute("aria-colcount", String(cols + 1));
  await select(page, 2, 27);
  await type(page, "in the table");
  await page.keyboard.press("ArrowUp");
  await expect(table.getByRole("columnheader")).toHaveText("AB");
  await expect(table.getByRole("columnheader")).toHaveAttribute("aria-colindex", "29");
  await expect(table.getByRole("rowheader")).toHaveText("3");
  await expect(table.locator("tbody tr")).toHaveAttribute("aria-rowindex", "4");
  await expect(table.locator("tbody td")).toHaveText("in the table");
  // Nothing in it takes the keyboard, or the pointer.
  expect(await table.locator("a, button, input, [tabindex], [contenteditable]").count()).toBe(0);

  const editor = grid.getByRole("textbox", { name: "Cell editor" });
  await expect(editor).toBeFocused();
  await expect(editor).toHaveAttribute("aria-multiline", "false");
  expect(await editor.getAttribute("aria-readonly")).toBeNull();
  expect(await editor.getAttribute("aria-describedby")).toBeNull();
  // The picture is passed over, and so are the bars that only a pointer can use.
  const shown = grid.locator("canvas, .reticula-scrollbar, .reticula-scrollbar-corner");
  for (const part of await shown.all()) {
    await expect(part).toHaveAttribute("aria-hidden", "true");
  }
  // The editor is the one stop in the grid.
  await expect(grid).toHaveAttribute("tabindex", "-1");
});

test("the input says when it is not for writing", async ({ page }) => {
  await page.goto("/app?hooks&lang=en");
  await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
  const editor = page.getByRole("textbox", { name: "Cell editor" });
  expect(await editor.getAttribute("aria-readonly")).toBeNull();
  await page.getByTestId("ai-panel-btn").click();
  await page.getByTestId("ai-demo-regional-sales-summary").click();
  await expect(page.getByTestId("ai-draft-banner")).toBeVisible();
  await expect(editor).toHaveAttribute("aria-readonly", "true");
  await page.getByTestId("ai-draft-banner").getByRole("button").first().click();
  await expect(page.getByTestId("ai-draft-banner")).toBeHidden();
  await expect(editor).not.toHaveAttribute("aria-readonly");
});

test.describe("a grid without an editor", () => {
  const draft = async (page: Page) => [
    await page.getByTestId("sim-row-0").inputValue(),
    await page.getByTestId("sim-col-0").inputValue(),
  ];

  async function openLab(page: Page): Promise<Locator> {
    await page.goto("/sim?lang=en");
    await expect(page.getByTestId("sim-convergence-badge")).toHaveText("All Replicas Converged", {
      timeout: 20_000,
    });
    return page.getByRole("application", { name: "Worksheet of Alice" });
  }

  test("has its names in the language of the page", async ({ page }) => {
    await page.goto("/sim?lang=zh");
    const grid = page.getByRole("application", { name: "Alice 的工作表" });
    await expect(grid).toBeVisible({ timeout: 20_000 });
    await expect(grid.getByRole("table", { name: "选中的单元格" })).toBeAttached();
    await grid.focus();
    await page.keyboard.press("ArrowDown");
    await expectSaid(page, "A2，空");
  });

  test("has a name, and moves its selection with the keys", async ({ page }) => {
    const grid = await openLab(page);
    await expect(grid).toHaveAttribute("data-testid", "sim-grid-0");
    await expect(page.getByRole("application", { name: "Worksheet of Bob" })).toBeVisible();
    await tabTo(page, grid);
    // Where the keyboard is can be seen.
    await expect(grid).toHaveCSS("outline-style", "solid");

    const table = grid.getByRole("table", { name: "Selected cell" });
    const last = async (axis: "row" | "col") =>
      Number(await table.getAttribute(`aria-${axis}count`)) - 2;
    const lastRow = await last("row");
    for (const [key, row, col] of [
      ["ArrowDown", 1, 0],
      ["ArrowRight", 1, 1],
      ["End", 1, await last("col")],
      ["ControlOrMeta+Home", 0, 0],
      ["ControlOrMeta+ArrowDown", lastRow, 0],
      ["ArrowUp", lastRow - 1, 0],
      ["PageDown", lastRow, 0],
    ] as const) {
      await page.keyboard.press(key);
      await expect.poll(() => draft(page), key).toEqual([String(row), String(col)]);
    }
    await page.keyboard.press("PageUp");
    await expect.poll(async () => Number((await draft(page))[0])).toBeLessThan(lastRow - 1);
    const [row] = await draft(page);
    await expect(table.getByRole("rowheader")).toHaveText(String(Number(row) + 1));
    await expectSaid(page, `A${Number(row) + 1}, blank`);
    await page.keyboard.press("Shift+ArrowDown");
    await expectSaid(page, `A${Number(row) + 1} to A${Number(row) + 2} selected, 2 cells`);

    // What the keys do not move with is the page's.
    await pressTab(page);
    expect(await grid.evaluate((host) => host.contains(document.activeElement))).toBe(false);
  });

  test("follows its selection, and each of them has a table of its own", async ({ page }) => {
    const grid = await openLab(page);
    const scrolled = () =>
      grid
        .locator(".reticula-scrollbar-thumb")
        .evaluateAll((thumbs) =>
          thumbs.map((thumb) => thumb instanceof HTMLElement && thumb.style.cssText),
        );
    const before = await scrolled();
    await grid.focus();
    await page.keyboard.press("ControlOrMeta+End");
    await expect.poll(scrolled).not.toEqual(before);
    const ids = await page.evaluate(() =>
      [...document.querySelectorAll("[id]")].map(({ id }) => id),
    );
    expect(new Set(ids).size).toBe(ids.length);
    expect(ids.filter((id) => /^reticula-mirror-\d+$/.test(id))).toHaveLength(2);
    // The cell's headers are those of its own table.
    const cell = grid.locator(".reticula-mirror tbody td");
    const headers = (await cell.getAttribute("headers"))?.split(" ") ?? [];
    expect(headers).toHaveLength(2);
    for (const id of headers) {
      await expect(grid.locator(`#${id}`)).toHaveCount(1);
    }
  });
});
