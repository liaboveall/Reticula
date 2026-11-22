import { readFile } from "node:fs/promises";
import { expect, type Page, test } from "@playwright/test";
import {
  type CollabServerHandle,
  createAndShareSheet,
  editCell,
  joinViaSharePath,
  settled,
  signInDev,
  startCollabServer,
} from "./collab-helpers.ts";

async function openWorkbook(page: Page, docQuery: string): Promise<void> {
  await page.goto(`/app?hooks&${docQuery}`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await settled(page);
}

async function gridOriginAndMetrics(page: Page) {
  const box = await page.getByTestId("grid").boundingBox();
  if (!box) {
    throw new Error("missing grid bounding box");
  }
  const metrics = await page.evaluate(() => window.__reticula?.stats().metrics);
  if (!metrics) {
    throw new Error("missing grid metrics");
  }
  return { box, metrics };
}

async function clickCell(
  page: Page,
  row: number,
  col: number,
  options?: { button?: "left" | "right" },
): Promise<void> {
  const { box, metrics } = await gridOriginAndMetrics(page);
  await page.mouse.click(
    box.x + metrics.headerWidth + metrics.colWidth * (col + 0.5),
    box.y + metrics.headerHeight + metrics.rowHeight * (row + 0.5),
    options?.button ? { button: options.button } : undefined,
  );
}

async function editCellViaBar(
  page: Page,
  row: number,
  col: number,
  address: string,
  raw: string,
  expectedText?: string,
): Promise<void> {
  const beforeSeq = await page.evaluate(() => Number(window.__reticula?.docStatus().walSeq ?? "0"));
  await page.evaluate(
    ([r, c]) => {
      window.__reticula?.select(r, c);
    },
    [row, col] as const,
  );
  await expect(page.getByTestId("name-box")).toHaveText(address);
  const formulaInput = page.getByTestId("formula-input");
  await expect(formulaInput).toHaveAttribute("contenteditable", "true");
  await formulaInput.fill(raw);
  await formulaInput.press("Enter");
  await page.waitForFunction(
    (seq) => Number(window.__reticula?.docStatus().walSeq ?? "0") > seq,
    beforeSeq,
  );
  if (expectedText !== undefined) {
    await expect
      .poll(() => page.evaluate(([r, c]) => window.__reticula?.textAt(r, c), [row, col] as const))
      .toBe(expectedText);
  }
  await settled(page);
}

async function clearCellViaContext(page: Page, row: number, col: number): Promise<void> {
  const beforeSeq = await page.evaluate(() => Number(window.__reticula?.docStatus().walSeq ?? "0"));
  await clickCell(page, row, col);
  await page.keyboard.press("Delete");
  await page.waitForFunction(
    (seq) => Number(window.__reticula?.docStatus().walSeq ?? "0") > seq,
    beforeSeq,
  );
  await settled(page);
}

function crc32(bytes: Uint8Array): number {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) {
      crc = crc & 1 ? 0xedb88320 ^ (crc >>> 1) : crc >>> 1;
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function packStoreZip(entries: readonly { readonly name: string; readonly xml: string }[]): Buffer {
  const enc = new TextEncoder();
  const encoded = entries.map((entry) => {
    const nameBytes = enc.encode(entry.name);
    const data = enc.encode(entry.xml);
    return { nameBytes, data, crc: crc32(data), offset: 0 };
  });

  let localSize = 0;
  let centralSize = 0;
  for (const item of encoded) {
    item.offset = localSize;
    localSize += 30 + item.nameBytes.byteLength + item.data.byteLength;
    centralSize += 46 + item.nameBytes.byteLength;
  }

  const out = Buffer.alloc(localSize + centralSize + 22);
  let pos = 0;
  for (const item of encoded) {
    out.writeUInt32LE(0x04034b50, pos);
    out.writeUInt16LE(20, pos + 4);
    out.writeUInt16LE(0x0800, pos + 6);
    out.writeUInt16LE(0, pos + 8);
    out.writeUInt16LE(0, pos + 10);
    out.writeUInt16LE(0x21, pos + 12);
    out.writeUInt32LE(item.crc, pos + 14);
    out.writeUInt32LE(item.data.byteLength, pos + 18);
    out.writeUInt32LE(item.data.byteLength, pos + 22);
    out.writeUInt16LE(item.nameBytes.byteLength, pos + 26);
    out.writeUInt16LE(0, pos + 28);
    out.set(item.nameBytes, pos + 30);
    out.set(item.data, pos + 30 + item.nameBytes.byteLength);
    pos += 30 + item.nameBytes.byteLength + item.data.byteLength;
  }

  const centralStart = pos;
  for (const item of encoded) {
    out.writeUInt32LE(0x02014b50, pos);
    out.writeUInt16LE(20, pos + 4);
    out.writeUInt16LE(20, pos + 6);
    out.writeUInt16LE(0x0800, pos + 8);
    out.writeUInt16LE(0, pos + 10);
    out.writeUInt16LE(0, pos + 12);
    out.writeUInt16LE(0x21, pos + 14);
    out.writeUInt32LE(item.crc, pos + 16);
    out.writeUInt32LE(item.data.byteLength, pos + 20);
    out.writeUInt32LE(item.data.byteLength, pos + 24);
    out.writeUInt16LE(item.nameBytes.byteLength, pos + 28);
    out.writeUInt16LE(0, pos + 30);
    out.writeUInt16LE(0, pos + 32);
    out.writeUInt16LE(0, pos + 34);
    out.writeUInt16LE(0, pos + 36);
    out.writeUInt32LE(0, pos + 38);
    out.writeUInt32LE(item.offset, pos + 42);
    out.set(item.nameBytes, pos + 46);
    pos += 46 + item.nameBytes.byteLength;
  }

  out.writeUInt32LE(0x06054b50, pos);
  out.writeUInt16LE(0, pos + 4);
  out.writeUInt16LE(0, pos + 6);
  out.writeUInt16LE(encoded.length, pos + 8);
  out.writeUInt16LE(encoded.length, pos + 10);
  out.writeUInt32LE(centralSize, pos + 12);
  out.writeUInt32LE(centralStart, pos + 16);
  out.writeUInt16LE(0, pos + 20);
  return out;
}

function buildMinimalXlsxWithOpaqueFormula(): Buffer {
  return packStoreZip([
    {
      name: "[Content_Types].xml",
      xml:
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">' +
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>' +
        '<Default Extension="xml" ContentType="application/xml"/>' +
        '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>' +
        '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>' +
        '<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>' +
        "</Types>",
    },
    {
      name: "_rels/.rels",
      xml:
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' +
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>' +
        "</Relationships>",
    },
    {
      name: "xl/workbook.xml",
      xml:
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
        '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" ' +
        'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">' +
        '<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>' +
        "</workbook>",
    },
    {
      name: "xl/_rels/workbook.xml.rels",
      xml:
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' +
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>' +
        '<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>' +
        "</Relationships>",
    },
    {
      name: "xl/styles.xml",
      xml:
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
        '<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">' +
        '<fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>' +
        '<fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills>' +
        '<borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders>' +
        '<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>' +
        '<cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs>' +
        "</styleSheet>",
    },
    {
      name: "xl/worksheets/sheet1.xml",
      xml:
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
        '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">' +
        "<sheetData>" +
        '<row r="1"><c r="A1"><v>100</v></c><c r="B1"><f>SUM(Table1[Amount])</f><v>250.5</v></c></row>' +
        "</sheetData>" +
        "</worksheet>",
    },
  ]);
}

test("dynamic array spills, #SPILL! symmetric collision & recovery, A1#, and SPILL badge work", async ({
  page,
}) => {
  await openWorkbook(page, "doc=m3a-spill");

  await editCellViaBar(page, 0, 0, "A1", "=SEQUENCE(3)", "1");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 0))).toBe("2");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(2, 0))).toBe("3");

  await page.evaluate(() => window.__reticula?.select(1, 0));
  await expect(page.getByTestId("name-box")).toHaveText("A2");
  await expect(page.getByTestId("spill-badge")).toBeVisible();
  await expect(page.getByTestId("formula-input")).toHaveText("");

  await page.evaluate(() => window.__reticula?.select(0, 0));
  await expect(page.getByTestId("name-box")).toHaveText("A1");
  await expect(page.getByTestId("spill-badge")).toHaveCount(0);
  await expect(page.getByTestId("formula-input")).toHaveText("=SEQUENCE(3)");

  await editCellViaBar(page, 0, 2, "C1", "=SUM(A1#)", "6");

  // Blocking A2 forces #SPILL! on A1 and clears spilled A3.
  await editCellViaBar(page, 1, 0, "A2", "99", "99");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("#SPILL!");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(2, 0))).toBe("");

  // Clearing A2 restores the dynamic spill and downstream # reference.
  await clearCellViaContext(page, 1, 0);
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("1");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 0))).toBe("2");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(2, 0))).toBe("3");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 2))).toBe("6");

  // Two overlapping spills (`E2` -> `E2:G2`, `F1` -> `F1:F3`) intersect symmetrically at `F2`.
  await editCellViaBar(page, 1, 4, "E2", "=SEQUENCE(1,3)", "1");
  await editCellViaBar(page, 0, 5, "F1", "=SEQUENCE(3)");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 4))).toBe("#SPILL!");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 5))).toBe("#SPILL!");

  await clearCellViaContext(page, 1, 4);
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 5))).toBe("1");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 5))).toBe("2");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(2, 5))).toBe("3");
});

test("two-level named ranges (workbook + sheet scope shadowing, delete, undo)", async ({
  page,
}) => {
  await openWorkbook(page, "doc=m3a-names");

  await page.getByTestId("m3a-names-btn").click();
  const namesModal = page.getByTestId("m3a-names-modal");
  await expect(namesModal).toBeVisible();

  await page.getByTestId("name-input").fill("TaxRate");
  await page.getByTestId("name-formula-input").fill("=0.1");
  await page.getByTestId("name-save-btn").click();
  await expect(page.getByTestId("names-list")).toContainText("TaxRate (Workbook)");
  await namesModal.getByRole("button", { name: "Close" }).click();
  await expect(namesModal).toBeHidden();

  await editCellViaBar(page, 0, 0, "A1", "=100*TaxRate", "10");

  await page.getByTestId("add-sheet-btn").click();
  await expect(page.getByTestId("sheet-tab-1")).toHaveText("Sheet2");
  await settled(page);

  await page.getByTestId("m3a-names-btn").click();
  await expect(namesModal).toBeVisible();
  await page.getByTestId("name-input").fill("TaxRate");
  await page.getByTestId("name-formula-input").fill("=0.25");
  await namesModal.getByLabel("Sheet2 only").check();
  await page.getByTestId("name-save-btn").click();
  await expect(page.getByTestId("names-list")).toContainText("TaxRate (Sheet2)");
  await namesModal.getByRole("button", { name: "Close" }).click();

  await editCellViaBar(page, 0, 0, "A1", "=100*TaxRate", "25");

  await page.getByTestId("sheet-tab-0").click();
  await expect(page.getByTestId("sheet-tab-0")).toHaveAttribute("aria-selected", "true");
  await settled(page);
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("10");

  await page.getByTestId("sheet-tab-1").click();
  await expect(page.getByTestId("sheet-tab-1")).toHaveAttribute("aria-selected", "true");
  await settled(page);

  // Deleting sheet-scoped `TaxRate` falls back to workbook-scoped `TaxRate` (0.1).
  await page.getByTestId("m3a-names-btn").click();
  await expect(namesModal).toBeVisible();
  await page
    .getByTestId("names-list")
    .locator(".m3a-match", { hasText: "(Sheet2)" })
    .getByTestId("name-delete-TaxRate")
    .click();
  await expect(page.getByTestId("names-list")).not.toContainText("(Sheet2)");
  await namesModal.getByRole("button", { name: "Close" }).click();
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("10");

  // Deleting workbook-scoped `TaxRate` yields `#NAME?`, and Undo restores it.
  await page.getByTestId("m3a-names-btn").click();
  await expect(namesModal).toBeVisible();
  await page.getByTestId("name-delete-TaxRate").click();
  await expect(page.getByTestId("names-list")).toContainText("No defined named ranges");
  await namesModal.getByRole("button", { name: "Close" }).click();
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("#NAME?");

  await page.getByTestId("undo-btn").click();
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("10");
});

test("local AutoFilter projection, physical row mapping, SUBTOTAL, and invariant StateDigest", async ({
  page,
}) => {
  await openWorkbook(page, "doc=m3a-filter");

  await editCellViaBar(page, 0, 0, "A1", "Item", "Item");
  await editCellViaBar(page, 0, 1, "B1", "Amount", "Amount");
  await editCellViaBar(page, 1, 0, "A2", "Alpha", "Alpha");
  await editCellViaBar(page, 1, 1, "B2", "10", "10");
  await editCellViaBar(page, 2, 0, "A3", "Beta", "Beta");
  await editCellViaBar(page, 2, 1, "B3", "20", "20");
  await editCellViaBar(page, 3, 0, "A4", "Gamma", "Gamma");
  await editCellViaBar(page, 3, 1, "B4", "30", "30");
  await editCellViaBar(page, 4, 0, "A5", "Delta", "Delta");
  await editCellViaBar(page, 4, 1, "B5", "40", "40");
  await editCellViaBar(page, 0, 3, "D1", "=SUBTOTAL(109,B2:B5)", "100");

  const digestBefore = await page.evaluate(() => window.__reticula?.stateDigest());
  expect(digestBefore).toBeTruthy();

  await page.evaluate(() => window.__reticula?.selectRange({ row: 0, col: 0 }, { row: 4, col: 1 }));
  await expect(page.getByTestId("name-box")).toHaveText("A1:B5");
  await page.getByTestId("m3a-filter-btn").click();
  await expect(page.getByTestId("m3a-filter-btn")).toHaveClass(/\bactive\b/);

  // Click the header chevron in column B (`row = 0, col = 1`) at its right edge.
  const { box, metrics } = await gridOriginAndMetrics(page);
  await page.mouse.click(
    box.x + metrics.headerWidth + metrics.colWidth * 2 - 10,
    box.y + metrics.headerHeight + metrics.rowHeight * 0.5,
  );
  const filterModal = page.getByTestId("m3a-filter-modal");
  await expect(filterModal).toBeVisible();

  await filterModal.getByRole("button", { name: "Number" }).click();
  await page.getByTestId("filter-num-val").fill("20");
  await page.getByTestId("filter-apply-btn").click();
  await expect(filterModal).toBeHidden();

  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 0))).toBe("Gamma");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 1))).toBe("30");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(2, 0))).toBe("Delta");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(2, 1))).toBe("40");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 3))).toBe("70");

  await page.evaluate(() => window.__reticula?.select(1, 0));
  await expect
    .poll(() => page.evaluate(() => window.__reticula?.stats().selection?.rowId))
    .toBe("3");

  const digestFiltered = await page.evaluate(() => window.__reticula?.stateDigest());
  expect(digestFiltered).toBe(digestBefore);

  // Turning off AutoFilter restores all rows and full SUBTOTAL sum.
  await page.getByTestId("m3a-filter-btn").click();
  await expect(page.getByTestId("m3a-filter-btn")).not.toHaveClass(/\bactive\b/);
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 0))).toBe("Alpha");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 3))).toBe("100");
});

test("Find & Replace supports regex, formula mode, jump-to-match, and atomic undo", async ({
  page,
}) => {
  await openWorkbook(page, "doc=m3a-find");

  await editCellViaBar(page, 0, 0, "A1", "item-10", "item-10");
  await editCellViaBar(page, 1, 0, "A2", "item-20", "item-20");
  await editCellViaBar(page, 2, 0, "A3", "other", "other");
  await editCellViaBar(page, 0, 1, "B1", "=10+1", "11");
  await editCellViaBar(page, 1, 1, "B2", "=10+2", "12");

  await page.getByTestId("m3a-find-btn").click();
  const findModal = page.getByTestId("m3a-find-modal");
  await expect(findModal).toBeVisible();

  await page.getByTestId("find-query").fill("item-\\d+");
  await page.getByTestId("find-regex").check();
  await page.getByTestId("find-all-btn").click();
  await expect(page.getByTestId("find-status")).toContainText("Found 2");

  const matchButtons = page.getByTestId("find-matches-list").locator("button");
  await expect(matchButtons).toHaveCount(2);
  await matchButtons.nth(1).click();
  await expect(page.getByTestId("name-box")).toHaveText("A2");

  await findModal.getByRole("button", { name: "Replace…" }).click();
  await page.getByTestId("replace-input").fill("SKU");
  await page.getByTestId("replace-all-btn").click();
  await expect(page.getByTestId("find-status")).toContainText("Replaced 2");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("SKU");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 0))).toBe("SKU");
  expect(await page.evaluate(() => window.__reticula?.textAt(2, 0))).toBe("other");

  // Formula replace across `=10+1` and `=10+2` -> `=100+1` and `=100+2`.
  await page.getByTestId("find-in").selectOption("formulas");
  await page.getByTestId("find-regex").uncheck();
  await page.getByTestId("find-query").fill("10");
  await page.getByTestId("replace-input").fill("100");
  await page.getByTestId("replace-all-btn").click();
  await expect(page.getByTestId("find-status")).toContainText("Replaced 2");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 1))).toBe("101");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 1))).toBe("102");

  await findModal.getByRole("button", { name: "Close" }).click();
  await expect(findModal).toBeHidden();

  // Single undo reverts both formula replacements atomically; second undo reverts both text ones.
  await page.getByTestId("undo-btn").click();
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 1))).toBe("11");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 1))).toBe("12");

  await page.getByTestId("undo-btn").click();
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("item-10");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 0))).toBe("item-20");
});

test("MoveCells cut-paste tracks references onto moved cells and preserves moved formula internals", async ({
  page,
}) => {
  await openWorkbook(page, "doc=m3a-move");

  await editCellViaBar(page, 0, 0, "A1", "10", "10");
  await editCellViaBar(page, 0, 2, "C1", "=A1*2", "20");
  await editCellViaBar(page, 2, 0, "A3", "=A1+5", "15");

  // Cut A1 -> paste onto B2: dependents rewrite `A1` -> `B2` cleanly.
  await clickCell(page, 0, 0, { button: "right" });
  await page.getByTestId("ctx-cut").click();
  await clickCell(page, 1, 1, { button: "right" });
  await page.getByTestId("ctx-paste").click();

  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 1))).toBe("10");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 2))).toBe("20");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(2, 0))).toBe("15");

  await page.evaluate(() => window.__reticula?.select(0, 2));
  await expect(page.getByTestId("formula-input")).toHaveText("=B2*2");
  await page.evaluate(() => window.__reticula?.select(2, 0));
  await expect(page.getByTestId("formula-input")).toHaveText("=B2+5");

  // Cut formula cell A3 (`=B2+5`) -> paste onto D3: internal ref `=B2+5` stays anchored.
  await clickCell(page, 2, 0, { button: "right" });
  await page.getByTestId("ctx-cut").click();
  await clickCell(page, 2, 3, { button: "right" });
  await page.getByTestId("ctx-paste").click();

  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(2, 3))).toBe("15");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(2, 0))).toBe("");
  await page.evaluate(() => window.__reticula?.select(2, 3));
  await expect(page.getByTestId("formula-input")).toHaveText("=B2+5");
});

test("XLSX import/export preserves dynamic & opaque formulas and unauthenticated /benchmarks renders SVG charts", async ({
  page,
}) => {
  const pageErrors: string[] = [];
  page.on("pageerror", (err) => pageErrors.push(err.message));

  // Part A: XLSX round-trip including preserved unknown/opaque formulas + cached values.
  await openWorkbook(page, "doc=m3a-xlsx");
  await editCellViaBar(page, 0, 0, "A1", "a", "a");
  await editCellViaBar(page, 1, 0, "A2", "b", "b");
  await editCellViaBar(page, 0, 1, "B1", "100", "100");
  await editCellViaBar(page, 1, 1, "B2", "250", "250");
  await editCellViaBar(page, 0, 2, "C1", '=XLOOKUP("b",A1:A2,B1:B2)', "250");

  await page.getByTestId("workbooks-btn").click();
  await expect(page.getByTestId("workbooks-drawer")).toBeVisible();

  const dlPromise = page.waitForEvent("download");
  await page.getByTestId("drawer-export-xlsx").click();
  const dl = await dlPromise;
  expect(dl.suggestedFilename()).toMatch(/\.xlsx$/);
  const dlPath = await dl.path();
  expect(dlPath).toBeTruthy();
  if (!dlPath) {
    throw new Error("missing exported xlsx download path");
  }
  const exportedXlsx = await readFile(dlPath);
  expect(exportedXlsx.subarray(0, 4).toString("binary")).toBe("PK\x03\x04");

  await page.getByTestId("import-file-input").setInputFiles({
    name: "roundtrip.xlsx",
    mimeType: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    buffer: exportedXlsx,
  });
  await expect(page.getByTestId("workbooks-drawer")).toHaveCount(0);
  await expect(page.getByTestId("document")).toContainText("roundtrip");
  await settled(page);
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 2))).toBe("250");
  await page.evaluate(() => window.__reticula?.select(0, 2));
  await expect(page.getByTestId("formula-input")).toHaveText('=XLOOKUP("b",A1:A2,B1:B2)');

  // Import minimal XLSX with structured-ref formula `SUM(Table1[Amount])` and cached `250.5`.
  const opaqueInput = buildMinimalXlsxWithOpaqueFormula();
  await page.getByTestId("workbooks-btn").click();
  await expect(page.getByTestId("workbooks-drawer")).toBeVisible();
  await page.getByTestId("import-file-input").setInputFiles({
    name: "opaque-check.xlsx",
    mimeType: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    buffer: opaqueInput,
  });
  await expect(page.getByTestId("workbooks-drawer")).toHaveCount(0);
  await expect(page.getByTestId("document")).toContainText("opaque-check");
  await settled(page);
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("100");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 1))).toBe("250.5");
  await page.evaluate(() => window.__reticula?.select(0, 1));
  await expect(page.getByTestId("formula-input")).toHaveText("=SUM(Table1[Amount])");

  // Re-export and re-import to confirm opaque formula and cached value round-trip intact.
  await page.getByTestId("workbooks-btn").click();
  const reExportPromise = page.waitForEvent("download");
  await page.getByTestId("drawer-export-xlsx").click();
  const reExportDl = await reExportPromise;
  const reExportPath = await reExportDl.path();
  expect(reExportPath).toBeTruthy();
  if (!reExportPath) {
    throw new Error("missing re-exported xlsx path");
  }
  const reExportedBuf = await readFile(reExportPath);
  await page.getByTestId("import-file-input").setInputFiles({
    name: "opaque-reimport.xlsx",
    mimeType: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    buffer: reExportedBuf,
  });
  await expect(page.getByTestId("workbooks-drawer")).toHaveCount(0);
  await expect(page.getByTestId("document")).toContainText("opaque-reimport");
  await settled(page);
  await expect(page.getByTestId("quota-banner")).toContainText("preserved with cached values");
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 1))).toBe("250.5");
  await page.evaluate(() => window.__reticula?.select(0, 1));
  await expect(page.getByTestId("formula-input")).toHaveText("=SUM(Table1[Amount])");

  // Part B: Unauthenticated `/benchmarks` page renders SVG charts, tables, and diff report.
  await page.goto("/");
  await page.getByTestId("nav-benchmarks").click();
  await expect(page).toHaveURL(/\/benchmarks$/);
  await expect(page.getByTestId("benchmarks-page")).toBeVisible();
  await expect(page.getByTestId("bench-overview")).toBeVisible();
  await expect(page.getByTestId("bench-svg-engine")).toBeVisible();
  await expect(page.getByTestId("bench-svg-b9")).toBeVisible();
  await expect(page.getByTestId("bench-svg-diff")).toBeVisible();
  await expect(page.getByTestId("bench-table")).toBeVisible();
  await expect(page.getByTestId("bench-diff-section")).toContainText("51,250");
  await expect(page.getByTestId("bench-diff-section")).toContainText("99.65%");
  await expect(page.getByTestId("bench-divergences-table")).toBeVisible();

  expect(await page.locator('[data-testid="bench-svg-engine"] rect').count()).toBeGreaterThan(2);
  expect(await page.locator('[data-testid="bench-svg-b9"] circle').count()).toBeGreaterThan(2);
  expect(await page.locator('[data-testid="bench-svg-diff"] rect').count()).toBeGreaterThan(2);

  await page.getByRole("button", { name: "Chrome WASM" }).click();
  expect(await page.locator('[data-testid="bench-table"] tbody tr').count()).toBeGreaterThan(0);

  expect(pageErrors).toEqual([]);
});

test.describe("M3a collaborative concurrent sort convergence (E16)", () => {
  test.describe.configure({ mode: "serial" });

  let server: CollabServerHandle | null = null;

  test.beforeAll(async () => {
    server = await startCollabServer();
  });

  test.afterAll(async () => {
    await server?.stop();
    server = null;
  });

  test("online multi-key sort commutes with offline row edit onto sorted row position", async ({
    browser,
  }, testInfo) => {
    test.setTimeout(90_000);
    if (!server) {
      throw new Error("server not started");
    }

    const aliceCtx = await browser.newContext();
    const bobCtx = await browser.newContext();
    try {
      const alice = await aliceCtx.newPage();
      const bob = await bobCtx.newPage();

      await signInDev(alice, server.baseUrl, "alice");
      const { joinPath } = await createAndShareSheet(
        alice,
        `Concurrent Sort ${testInfo.project.name}-${Date.now()}`,
      );
      await joinViaSharePath(bob, server.baseUrl, joinPath, "bob");

      await editCell(alice, 0, 0, "A1", "Item");
      await editCell(alice, 0, 1, "B1", "Score");
      await editCell(alice, 1, 0, "A2", "Alpha");
      await editCell(alice, 1, 1, "B2", "10");
      await editCell(alice, 2, 0, "A3", "Beta");
      await editCell(alice, 2, 1, "B3", "30");
      await editCell(alice, 3, 0, "A4", "Gamma");
      await editCell(alice, 3, 1, "B4", "20");
      await expect(alice.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 10_000,
      });

      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(3, 0)), { timeout: 5_000 })
        .toBe("Gamma");
      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(3, 1)), { timeout: 5_000 })
        .toBe("20");

      // Take Bob offline before Alice sorts A1:B4 descending by Score (`colOffset = 1`).
      await bob.evaluate(() => window.__reticula?.setCollabOffline(true));

      await alice.evaluate(() =>
        window.__reticula?.selectRange({ row: 0, col: 0 }, { row: 3, col: 1 }),
      );
      await expect(alice.getByTestId("name-box")).toHaveText("A1:B4");
      await alice.getByTestId("m3a-sort-btn").click();
      await expect(alice.getByTestId("m3a-sort-modal")).toBeVisible();
      await alice.getByTestId("sort-header-mode").selectOption("yes");
      await alice.getByTestId("sort-key-col-0").selectOption("1");
      await alice.getByTestId("sort-key-dir-0").selectOption("desc");
      await alice.getByTestId("sort-apply-btn").click();
      await expect(alice.getByTestId("m3a-sort-modal")).toBeHidden();

      await expect(alice.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 10_000,
      });
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(1, 0)), { timeout: 5_000 })
        .toBe("Beta");
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(2, 0)), { timeout: 5_000 })
        .toBe("Gamma");
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(3, 0)), { timeout: 5_000 })
        .toBe("Alpha");

      // While offline, Bob edits row 4 (`B4`, which belongs to `Gamma` on Bob's pre-sort grid).
      await editCell(bob, 3, 1, "B4", "25");

      // Reconnect Bob: both clients must converge on identical digest with Gamma = 25 at row 3 (`B3`).
      await bob.evaluate(() => window.__reticula?.setCollabOffline(false));
      await expect(bob.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 10_000,
      });
      await expect(alice.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 10_000,
      });
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.collabStatus().pending), {
          timeout: 5_000,
        })
        .toBe(0);
      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.collabStatus().pending), {
          timeout: 5_000,
        })
        .toBe(0);

      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(2, 1)), { timeout: 5_000 })
        .toBe("25");
      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(2, 0)), { timeout: 5_000 })
        .toBe("Gamma");
      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(2, 1)), { timeout: 5_000 })
        .toBe("25");

      const aliceDigest = await alice.evaluate(() => window.__reticula?.stateDigest());
      const bobDigest = await bob.evaluate(() => window.__reticula?.stateDigest());
      expect(aliceDigest).toBeTruthy();
      expect(aliceDigest).toBe(bobDigest);
    } finally {
      await aliceCtx.close();
      await bobCtx.close();
    }
  });
});
