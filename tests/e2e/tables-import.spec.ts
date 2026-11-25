import { expect, type Page, test } from "@playwright/test";
import {
  canKeepTables,
  content,
  dropFiles,
  expectSaid,
  goTo,
  importTable,
  listenToPage,
  openImport,
  openWorkbook,
  ordersCsv,
  runCommand,
  selectedCell,
  tab,
  tableFiles,
  tableGrid,
  textsOf,
  view,
} from "./tables-helpers.ts";

// How files become data tables: the dialog, its preview, what goes wrong, and
// files that are dropped.

test.beforeEach(async ({ page }) => {
  await listenToPage(page);
  await openWorkbook(page);
});

async function choose(page: Page, name: string, data: string | Buffer): Promise<void> {
  await page.getByTestId("table-import-input").setInputFiles({
    name,
    mimeType: "application/octet-stream",
    buffer: typeof data === "string" ? Buffer.from(data) : data,
  });
}

test.describe("a file", () => {
  test.beforeEach(async ({ page }) => {
    test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
    await openImport(page);
  });

  test("is shown before it is imported, and read in another way if that is wanted", async ({
    page,
  }) => {
    await choose(page, "codes.csv", "code;size;when\n00123;1,5;2024-01-02\n00456;NA;2024-03-04\n");
    const preview = page.getByTestId("table-import-preview");
    await expect(page.getByTestId("table-import-delimiter")).toHaveValue(";");
    await expect(page.getByTestId("table-import-header")).toBeChecked();
    await expect(preview.locator("thead th")).toHaveCount(3);
    await expect(preview.locator("tbody tr")).toHaveCount(2);
    await expect(page.getByTestId("table-import-type-2")).toContainText("Automatic: Date");

    await page.getByTestId("table-import-header").uncheck();
    await expect(preview.locator("tbody tr")).toHaveCount(3);
    await page.getByTestId("table-import-header").check();
    await page.getByTestId("table-import-delimiter").selectOption(",");
    await expect(preview.locator("thead th")).toHaveCount(1);
    await page.getByTestId("table-import-delimiter").selectOption(";");
    await page.getByTestId("table-import-type-0").selectOption("integer");
    await page.getByTestId("table-import-blanks").check();
    await page.getByTestId("table-import-name").fill("parts");
    await page.getByTestId("table-import-start").click();
    await expect(tab(page, "parts")).toHaveAttribute("aria-selected", "true");
    // Codes that are told to be numbers have no zeros before them, and what is not one is blank.
    await expect(page.getByTestId("table-count")).toHaveText("2 rows");
    expect(await textsOf(page, "when", [1, 2])).toEqual(["2024-01-02", "2024-03-04"]);
  });

  test("has what became of it told once it is imported", async ({ page }) => {
    await choose(page, "odd.csv", "a,b\n1,x\n2\n3,y,z\nfour,w\n");
    await page.getByTestId("table-import-type-0").selectOption("integer");
    await page.getByTestId("table-import-start").click();
    const report = page.getByTestId("table-import-report");
    await expect(report.getByRole("listitem")).toHaveText([
      "1 fields were dropped: their rows have too many.",
      "1 rows were too short and were filled with blanks.",
      "a: 1 values did not fit the type and are blank.",
    ]);
    await page.getByTestId("table-import-show").click();
    await expect(view(page)).toHaveAttribute("data-status", "open");
    await runCommand(page, "tablePanel");
    await expect(page.getByTestId("table-report").getByRole("listitem")).toHaveCount(3);
    await page.reload();
    await expect(view(page)).toHaveAttribute("data-status", "open");
    await runCommand(page, "tablePanel");
    await expect(page.getByTestId("table-report").getByRole("listitem")).toHaveCount(3);
  });

  test("with a quote that is never closed is not imported, and why is said", async ({ page }) => {
    const rows = ordersCsv(40).split("\n");
    rows[20] = `20,"never closed,1.00,2024-01-01,`;
    await choose(page, "open.csv", rows.join("\n"));
    await page.getByTestId("table-import-start").click();
    await expect(page.getByTestId("table-import-failure")).toHaveText(
      "The quote that begins in row 21 is never closed.",
    );
    await page.keyboard.press("Escape");
    await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
    expect((await tableFiles(page)).filter((name) => name.endsWith(".rtbl"))).toEqual([]);
  });

  test("in another encoding is imported once that is chosen", async ({ page }) => {
    // GB18030 for 名称 and 北京.
    const bytes = Buffer.concat([
      Buffer.from([0xc3, 0xfb, 0xb3, 0xc6]),
      Buffer.from(",n\n"),
      Buffer.from([0xb1, 0xb1, 0xbe, 0xa9]),
      Buffer.from(",1\n"),
    ]);
    await choose(page, "gb.csv", bytes);
    await expect(page.getByTestId("table-import-failure")).toContainText("is not UTF-8");
    await page.getByTestId("table-import-encoding").selectOption("gb18030");
    await expect(page.getByTestId("table-import-failure")).toBeHidden();
    await expect(page.getByTestId("table-import-preview")).toContainText("北京");
    await page.getByTestId("table-import-start").click();
    await expect.poll(() => selectedCell(page)).toEqual({ column: "名称", row: "1", text: "北京" });
  });

  test("that is none of a table is told from others", async ({ page }) => {
    await choose(page, "broken.parquet", "PAR1 and nothing else");
    await expect(page.getByTestId("table-import-failure")).toHaveText(
      "This is not a Parquet file.",
    );
    await expect(page.getByTestId("table-import-start")).toHaveAttribute("aria-disabled", "true");
    await choose(page, "broken.rtbl", "no table");
    await page.getByTestId("table-import-start").click();
    await expect(page.getByTestId("table-import-failure")).toHaveText(
      "The file is damaged, or is not a data table.",
    );
    await choose(page, "empty.csv", "");
    await page.getByTestId("table-import-start").click();
    await expect(page.getByTestId("table-import-failure")).toHaveText(
      "There is nothing in the file that a data table could hold.",
    );
    await choose(page, "movie.mp4", "x");
    await expect(page.getByTestId("table-import-problem")).toHaveText(
      "Files of this kind cannot be opened: movie.mp4",
    );
  });

  test("is imported in the background when the dialog is closed, and can be cancelled", async ({
    page,
  }) => {
    await choose(page, "large.csv", ordersCsv(600_000));
    await expect(page.getByTestId("table-import-preview")).toBeVisible();
    await page.getByTestId("table-import-start").click();
    await expect(page.getByTestId("table-import-progress")).toBeVisible();
    await page.getByRole("button", { name: "Continue in the Background" }).click();
    await expect(page.getByTestId("table-import")).toBeHidden();
    await page.getByTestId("table-importing-cancel").click({ timeout: 5_000 });
    await expect(page.getByTestId("table-importing")).toBeHidden();
    await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
    await expect
      .poll(async () => (await tableFiles(page)).filter((name) => name.endsWith(".rtbl")))
      .toEqual([]);
  });
});

test.describe("a file that is dropped", () => {
  test("becomes a data table or a workbook, as is chosen", async ({ page }) => {
    test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
    const before = await content(page);
    await dropFiles(page.getByTestId("grid"), [{ name: "orders.csv", text: ordersCsv(50) }]);
    await expect(page.getByTestId("table-import")).toBeVisible();
    await expect(page.getByTestId("import-as-workbook")).toBeChecked();
    await page.getByTestId("import-as-table").check();
    await expect(page.getByTestId("table-import-preview")).toBeVisible();
    await page.getByTestId("table-import-start").click();
    await expect(tab(page, "orders")).toHaveAttribute("aria-selected", "true");
    expect(await content(page)).toEqual(before);

    await dropFiles(tableGrid(page), [{ name: "orders.csv", text: ordersCsv(50) }]);
    await page.getByTestId("table-import-start").click();
    await expect(page.getByTestId("document")).toContainText("orders");
    await expect(page.getByRole("tab")).toHaveText(["orders"]);
    await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(50, 0))).toBe("50");
  });

  test("becomes a workbook that nothing is typed into while it is read", async ({ page }) => {
    const column = (rows: number) =>
      page.evaluate(
        (count) => Array.from({ length: count }, (_, row) => window.__reticula?.textAt(row, 0)),
        rows,
      );
    await goTo(page, "A1");
    await page.keyboard.type("kept");
    await page.keyboard.press("Enter");
    await expect.poll(() => column(2)).toEqual(["kept", ""]);
    // The engine takes its time, as it does with a long file.
    await page.evaluate(() => window.__reticula?.spinWorker(2_500));
    await dropFiles(page.getByTestId("grid"), [{ name: "orders.csv", text: ordersCsv(50) }]);
    await page.getByTestId("table-import-start").click();
    await expect(page.getByTestId("csv-import-progress")).toBeVisible();
    await page.keyboard.type("typed meanwhile");
    await page.keyboard.press("Enter");
    await page.keyboard.press("Delete");
    await expect(page.getByTestId("document")).toContainText("orders");
    await expect(page.getByTestId("csv-import-progress")).toBeHidden();
    await expect.poll(() => column(3)).toEqual(["id", "1", "2"]);
    expect((await content(page)).operations).toBe("0");
    // From here on it can be changed.
    await page.evaluate(() => window.__reticula?.select(0, 0));
    await page.keyboard.type("changed");
    await page.keyboard.press("Enter");
    await expect.poll(() => column(2)).toEqual(["changed", "1"]);
    const { storageBackend } = (await page.evaluate(() => window.__reticula?.docStatus())) ?? {};
    if (storageBackend !== "memory") {
      await openWorkbook(page);
      await expect.poll(() => column(2)).toEqual(["kept", ""]);
    }
  });

  test("with many rows becomes a data table unless something else is chosen", async ({ page }) => {
    // It is estimated from its first rows: short ones, and a long comment that is never read.
    const lines = "1\n".repeat(1_100_000);
    await dropFiles(page.getByTestId("grid"), [{ name: "long.csv", text: `n\n${lines}` }]);
    await expect(page.getByTestId("import-as-table")).toBeChecked();
    await page.getByTestId("import-as-workbook").check();
    await expect(page.getByTestId("table-import-start")).not.toHaveAttribute(
      "aria-disabled",
      "true",
    );
  });

  test("on a formula is not read into it", async ({ page }) => {
    await page.getByTestId("grid").click();
    await page.keyboard.type("=1+");
    await dropFiles(page.locator(".reticula-editor .cm-content"), [
      { name: "a.csv", text: "dropped,text\n" },
    ]);
    await expect(page.getByTestId("table-import")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator(".formulabar .cm-content")).not.toContainText("dropped");
  });

  test("is refused while a dialog is open, with others, and where nothing may be changed", async ({
    page,
  }) => {
    const alert = page.locator('[role="alert"]');
    await runCommand(page, "keyboardHelp");
    await dropFiles(page.getByTestId("keyboard-help"), [{ name: "a.csv", text: "a\n1\n" }]);
    await expect(page.getByTestId("keyboard-help").locator('[role="alert"]')).toHaveText(
      "Close the dialog before you drop a file here.",
    );
    await page.keyboard.press("Escape");
    await dropFiles(page.getByTestId("grid"), [
      { name: "a.csv", text: "a\n1\n" },
      { name: "b.csv", text: "a\n1\n" },
    ]);
    await expect(alert.filter({ hasText: "Drop one file at a time." }).first()).toBeAttached();
    await openWorkbook(page, "&touch=1");
    await dropFiles(page.getByTestId("grid"), [{ name: "a.csv", text: "a\n1\n" }]);
    await expect(
      alert.filter({ hasText: "Nothing can be imported while the workbook is read-only." }).first(),
    ).toBeAttached();
    await expect(page.getByTestId("table-import")).toBeHidden();
  });

  test("of a kind that nothing can be made of is refused", async ({ page }) => {
    await dropFiles(page.getByTestId("grid"), [{ name: "photo.png", text: "x" }]);
    await expect(page.getByTestId("table-import-problem")).toHaveText(
      "Files of this kind cannot be opened: photo.png",
    );
  });
});

test.describe("the list of workbooks", () => {
  test.beforeEach(async ({ page }) => {
    test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
  });

  test("tells what tables take, and that exported workbooks have none", async ({ page }) => {
    await importTable(page, "orders", ordersCsv(1_000));
    await page.getByTestId("workbooks-btn").click();
    await expect(page.getByTestId("workbook-row-tables-doc")).toContainText(
      /1 data table, [\d.]+ KB/,
    );
    await expect(page.getByTestId("tables-not-exported")).toBeVisible();
    await page.keyboard.press("Escape");
    const download = page.waitForEvent("download");
    await page.getByTestId("export-rtcl-btn").click();
    await download;
    await expect(page.getByText("The file has no data tables in it.")).toBeVisible();
  });

  test("has tables imported into the workbook that is open", async ({ page }) => {
    await page.getByTestId("workbooks-btn").click();
    page.once("filechooser", () => {});
    await page.getByTestId("import-table-btn").click();
    await choose(page, "orders.csv", ordersCsv(10));
    await page.getByTestId("table-import-start").click();
    await expect(tab(page, "orders")).toHaveAttribute("aria-selected", "true");
  });

  test("deletes the tables with the workbook", async ({ page }) => {
    await importTable(page, "orders", ordersCsv(100));
    expect((await tableFiles(page)).length).toBeGreaterThan(0);
    await page.getByTestId("workbooks-btn").click();
    await page.getByTestId("delete-doc-tables-doc").click();
    await expect(page.getByTestId("workbook-row-tables-doc")).toBeHidden();
    expect(await tableFiles(page)).toEqual([]);
  });

  test("deletes no workbook whose tables are open in another tab", async ({ page, context }) => {
    await importTable(page, "orders", ordersCsv(100));
    const other = await context.newPage();
    await other.goto("/workbooks");
    await other.getByTestId("workbooks-tab-local").click();
    await expect(other.getByTestId("local-tables-tables-doc")).toContainText("1 data table");
    await other.getByRole("button", { name: /^Delete Local: tables-doc/ }).click();
    await expect(other.getByTestId("workbooks-error")).toContainText("open in another tab");
    expect((await tableFiles(page)).length).toBeGreaterThan(0);
    await page.close();
    await other.getByRole("button", { name: /^Delete Local: tables-doc/ }).click();
    await expect(other.getByTestId("local-wb-tables-doc")).toBeHidden();
    expect(await tableFiles(other)).toEqual([]);
  });
});

test("what is left of an import that was cut short is counted, and removed", async ({ page }) => {
  test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
  // There is a list of the tables before there is one.
  await importTable(page, "orders", ordersCsv(10));
  await runCommand(page, "tableDelete");
  await page.getByTestId("table-delete-confirm").click();
  await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
  await page.goto("/workbooks");
  await page.evaluate(async (doc) => {
    let dir = await navigator.storage.getDirectory();
    for (const name of ["reticula-v1", "tables", doc]) {
      dir = await dir.getDirectoryHandle(name);
    }
    const file = await dir.getFileHandle("cut-short.rtbl", { create: true });
    const writable = await file.createWritable();
    await writable.write(new Uint8Array(2_000_000));
    await writable.close();
  }, "tables-doc");
  await page.reload();
  await page.getByTestId("workbooks-tab-local").click();
  await expect(page.getByTestId("local-tables-tables-doc")).toContainText("1.91 MB");
  await openWorkbook(page);
  await expect.poll(() => tableFiles(page)).not.toContain("cut-short.rtbl");
  await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
});

test("a workbook whose tables are still to be uploaded is deleted when that is asked for twice", async ({
  page,
}) => {
  test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
  await importTable(page, "orders", ordersCsv(10));
  await page.goto("/workbooks");
  await page.evaluate(async (docId) => {
    let dir = await navigator.storage.getDirectory();
    for (const name of ["reticula-v1", "tables-queue"]) {
      dir = await dir.getDirectoryHandle(name, { create: true });
    }
    const file = await dir.getFileHandle("waiting.json", { create: true });
    const writable = await file.createWritable();
    const entry = { id: "waiting", name: "orders", rows: 10, cols: 5, bytes: 1 };
    const from = { docId, id: "any" };
    await writable.write(
      JSON.stringify({ userId: "u", docId: "cloud", entry, sha256: "", queuedAt: 1, from }),
    );
    await writable.close();
  }, "tables-doc");
  await page.getByTestId("workbooks-tab-local").click();
  const row = page.getByTestId("local-wb-tables-doc");
  await row.getByRole("button", { name: /^Delete Local/ }).click();
  await expect(page.getByTestId("workbooks-error")).toContainText("still being uploaded");
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: /^Delete Local/ }).click();
  await expect(row).toBeHidden();
  expect(await tableFiles(page)).toEqual([]);
  const queued = await page.evaluate(async () => {
    const opfs = await navigator.storage.getDirectory();
    const queue = await (await opfs.getDirectoryHandle("reticula-v1")).getDirectoryHandle(
      "tables-queue",
    );
    const names: string[] = [];
    for await (const name of (queue as unknown as { keys(): AsyncIterable<string> }).keys()) {
      names.push(name);
    }
    return names;
  });
  expect(queued).toEqual([]);
});

test("the workbook that takes the place of a deleted one has no tables", async ({ page }) => {
  // The other workbook there is, is listed by then.
  await page.evaluate(() => window.__reticula?.flushAndClose());
  await page.goto("/app?hooks");
  await expect(page.getByTestId("engine-status")).toHaveAttribute("data-status", "ready", {
    timeout: 20_000,
  });
  test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
  const sheets = await page.getByRole("tab").allTextContents();
  await importTable(page, "orders", ordersCsv(100));
  const docId = await page.evaluate(() => window.__reticula?.docStatus().docId);
  await page.getByTestId("workbooks-btn").click();
  await page.getByTestId("delete-doc-tables-doc").click();
  await expect(page.getByTestId("workbook-row-tables-doc")).toBeHidden();
  await page.getByTestId(`delete-doc-${docId}`).click();
  await expect.poll(() => tableFiles(page, docId)).toEqual([]);
  // It has the same name as the one before: the sample that there is when nothing else is.
  await expect(page.getByTestId("engine-status")).toHaveAttribute("data-status", "ready");
  expect(await page.evaluate(() => window.__reticula?.docStatus().docId)).toBe(docId);
  await expect(page.getByRole("tab")).toHaveText(sheets);
});

test.describe("where files cannot be kept", () => {
  test("that is said, and nothing is imported", async ({ page }) => {
    test.skip(await canKeepTables(page), "tables can be kept here");
    await openImport(page);
    await choose(page, "orders.csv", ordersCsv(10));
    await expect(page.getByTestId("table-import-preview")).toBeVisible();
    await page.getByTestId("table-import-start").click();
    await expect(page.getByTestId("table-import-failure")).toContainText(
      "This browser cannot keep data tables",
    );
    await page.keyboard.press("Escape");
    await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
  });
});

test("an import is told of as it goes on", async ({ page }) => {
  test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
  await importTable(page, "orders", ordersCsv(10));
  await expectSaid(page, /orders was imported: 10 rows/);
  await goTo(page, "id 10");
});
