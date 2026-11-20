import { readFile } from "node:fs/promises";
import { expect, type Page, test } from "@playwright/test";

async function settled(page: Page): Promise<void> {
  await page.waitForFunction(() => {
    const stats = window.__reticula?.stats();
    return stats !== undefined && stats.pagesHeld > 0 && stats.missingCells === 0;
  });
}

async function openWorkbook(page: Page, docQuery = ""): Promise<void> {
  const url = docQuery ? `/app?hooks&${docQuery}` : "/app?hooks";
  await page.goto(url);
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
  options?: { modifiers?: ("Control" | "Meta" | "Shift")[]; button?: "left" | "right" },
): Promise<void> {
  const { box, metrics } = await gridOriginAndMetrics(page);
  const modifiers = options?.modifiers ?? [];
  for (const mod of modifiers) {
    await page.keyboard.down(mod);
  }
  try {
    await page.mouse.click(
      box.x + metrics.headerWidth + metrics.colWidth * (col + 0.5),
      box.y + metrics.headerHeight + metrics.rowHeight * (row + 0.5),
      options?.button ? { button: options.button } : undefined,
    );
  } finally {
    for (let i = modifiers.length - 1; i >= 0; i--) {
      const mod = modifiers[i];
      if (mod) {
        await page.keyboard.up(mod);
      }
    }
  }
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

async function readOpfsWalLength(page: Page, docId: string): Promise<number | null> {
  return page.evaluate(async (id) => {
    if (!navigator.storage?.getDirectory) {
      return null;
    }
    try {
      const root = await navigator.storage.getDirectory();
      const v1 = await root.getDirectoryHandle("reticula-v1");
      const local = await v1.getDirectoryHandle("local");
      const dir = await local.getDirectoryHandle(id);
      const fh = await dir.getFileHandle("wal.log");
      const file = await fh.getFile();
      return file.size;
    } catch {
      return null;
    }
  }, docId);
}

test("formula language service, F4 toggle, cross-sheet point-and-click, sheet delete and undo work", async ({
  page,
}) => {
  await openWorkbook(page, "doc=sheets-e2e");

  // Add a second worksheet (`Sheet2`, id = 1) and enter `42` in `Sheet2!A1`.
  await page.getByTestId("add-sheet-btn").click();
  await expect(page.getByTestId("sheet-tab-1")).toHaveText("Sheet2");
  await settled(page);

  await editCellViaBar(page, 0, 0, "A1", "42", "42");
  expect(await page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("42");

  // Switch back to `Sheet1` (`id = 0`) and build `=SUM(Sheet2!$A$1)` in `B1` via autocomplete + point-and-click + F4.
  await page.getByTestId("sheet-tab-0").click();
  await expect(page.getByTestId("sheet-tab-0")).toHaveAttribute("aria-selected", "true");
  await settled(page);
  await page.evaluate(() => window.__reticula?.select(0, 1));
  await expect(page.getByTestId("name-box")).toHaveText("B1");

  const formulaInput = page.getByTestId("formula-input");
  await formulaInput.click();
  await page.keyboard.type("=SU");
  await expect(formulaInput).toHaveText("=SU");
  await expect(page.getByTestId("formula-completions")).toBeVisible();
  await expect(page.getByTestId("completion-SUM")).toBeVisible();
  await expect(page.getByTestId("completion-SQRT")).toHaveCount(0);

  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Tab");
  await expect(formulaInput).toHaveText("=SUM(");
  await expect(page.getByTestId("formula-signature")).toContainText("SUM");
  await expect(page.getByTestId("formula-diagnostic")).toBeVisible();

  // Click `Sheet2` tab while editing formula -> point-and-click `A1` -> toggle F4 -> close `)` and press Enter!
  await page.getByTestId("sheet-tab-1").click();
  await expect(page.getByTestId("sheet-tab-1")).toHaveAttribute("aria-selected", "true");
  await settled(page);
  await clickCell(page, 0, 0);
  await expect(formulaInput).toHaveText("=SUM(Sheet2!A1");

  await formulaInput.focus();
  await page.keyboard.press("F4");
  await expect(formulaInput).toHaveText("=SUM(Sheet2!$A$1");

  await page.keyboard.type(")");
  await page.keyboard.press("Enter");

  // Committing returns to host sheet (`Sheet1`) and evaluates `B1` to `42`.
  await expect(page.getByTestId("sheet-tab-0")).toHaveAttribute("aria-selected", "true");
  await settled(page);
  await page.waitForFunction(() => window.__reticula?.textAt(0, 1) === "42");

  // Delete `Sheet2` via tab context menu -> `B1` becomes `#REF!` (`=SUM(#REF!$A$1)`).
  await page.getByTestId("sheet-tab-1").click({ button: "right" });
  await page.getByTestId("ctx-delete-sheet").click();
  await expect(page.getByTestId("sheet-tab-1")).toHaveCount(0);
  await page.waitForFunction(() => window.__reticula?.textAt(0, 1) === "#REF!");

  await page.evaluate(() => window.__reticula?.select(0, 1));
  await expect(page.getByTestId("name-box")).toHaveText("B1");
  await expect(formulaInput).toHaveText("=SUM(#REF!$A$1)");

  // Undo (`UndeleteSheet`) restores `Sheet2` and recovers `B1` to `42`; Redo re-deletes; Undo restores again.
  await page.getByTestId("undo-btn").click();
  await expect(page.getByTestId("sheet-tab-1")).toHaveText("Sheet2");
  await page.waitForFunction(() => window.__reticula?.textAt(0, 1) === "42");

  await page.getByTestId("redo-btn").click();
  await page.waitForFunction(() => window.__reticula?.textAt(0, 1) === "#REF!");

  await page.getByTestId("undo-btn").click();
  await page.waitForFunction(() => window.__reticula?.textAt(0, 1) === "42");
});

test("clipboard copy/cut/paste shifts formulas, parses HTML tables, and multi-select formats & clears", async ({
  page,
}) => {
  await openWorkbook(page, "doc=clipboard-e2e");

  await editCellViaBar(page, 0, 0, "A1", "10");
  await editCellViaBar(page, 1, 0, "A2", "20");
  await editCellViaBar(page, 0, 1, "B1", "=A1*3");
  await page.waitForFunction(() => window.__reticula?.textAt(0, 1) === "30");

  // Copy `B1` via context menu and paste onto `B2` -> relative formula shifts to `=A2*3` = `60`.
  await clickCell(page, 0, 1, { button: "right" });
  await page.getByTestId("ctx-copy").click();
  await clickCell(page, 1, 1, { button: "right" });
  await page.getByTestId("ctx-paste").click();
  await page.waitForFunction(() => window.__reticula?.textAt(1, 1) === "60");
  await page.evaluate(() => window.__reticula?.select(1, 1));
  await expect(page.getByTestId("name-box")).toHaveText("B2");
  await expect(page.getByTestId("formula-input")).toHaveText("=A2*3");

  // Cut `A1` (`10`) and paste onto `C1` -> `A1` clears and `C1` receives `10`.
  await clickCell(page, 0, 0, { button: "right" });
  await page.getByTestId("ctx-cut").click();
  await clickCell(page, 0, 2, { button: "right" });
  await page.getByTestId("ctx-paste").click();
  await page.waitForFunction(() => window.__reticula?.textAt(0, 2) === "10");
  expect(await page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("");

  // Multi-select `A2` (`20`) and `C1` (`10`) with Ctrl+Click, apply Bold, then Delete both at once.
  await clickCell(page, 1, 0);
  await expect(page.getByTestId("name-box")).toHaveText("A2");
  await clickCell(page, 0, 2, { modifiers: ["Control"] });
  await page.getByTestId("fmt-bold").click();
  await expect(page.getByTestId("fmt-bold")).toHaveAttribute("aria-pressed", "true");

  await page.keyboard.press("Delete");
  await page.waitForFunction(
    () => window.__reticula?.textAt(1, 0) === "" && window.__reticula?.textAt(0, 2) === "",
  );

  // External HTML table paste (Excel / Google Sheets `text/html` clipboard format) onto `D1:E1`.
  await clickCell(page, 0, 3);
  await expect(page.getByTestId("name-box")).toHaveText("D1");
  await page.evaluate(() => {
    const active = document.activeElement ?? document.body;
    const dt = new DataTransfer();
    dt.setData("text/plain", "123.5\tExternal row");
    dt.setData(
      "text/html",
      '<table><tr><td style="font-weight:bold;text-align:right">123.5</td><td>External row</td></tr></table>',
    );
    const ev = new ClipboardEvent("paste", {
      clipboardData: dt,
      bubbles: true,
      cancelable: true,
    });
    if (!ev.clipboardData || ev.clipboardData.getData("text/plain") === "") {
      Object.defineProperty(ev, "clipboardData", { value: dt });
    }
    active.dispatchEvent(ev);
  });
  await page.waitForFunction(
    () =>
      window.__reticula?.textAt(0, 3) === "123.5" &&
      window.__reticula?.textAt(0, 4) === "External row",
  );
  await page.evaluate(() => window.__reticula?.select(0, 3));
  await expect(page.getByTestId("fmt-bold")).toHaveAttribute("aria-pressed", "true");
});

test("1,000,000-row formula fill handle encodes in constant O(1) WAL transaction bytes", async ({
  page,
}) => {
  await openWorkbook(page);

  // Insert a blank column `A` immediately to the left of the 1,000,000-row Order ID column (`B`).
  await page.evaluate(() => window.__reticula?.select(0, 0));
  await expect(page.getByTestId("name-box")).toHaveText("A1");
  await page.getByTestId("insert-col-left").click();
  await expect(page.getByTestId("document")).toContainText("1,000,000 rows × 23 columns");

  // Put `=F2*2` in `A2` (`row = 1, col = 0`).
  await editCellViaBar(page, 1, 0, "A2", "=F2*2");
  await page.waitForFunction(() => {
    const t = window.__reticula?.textAt(1, 0);
    return t !== null && t !== undefined && t.length > 0 && t !== "=F2*2";
  });

  // Wait for WAL flush and measure WAL size before 10-row drag fill.
  await page.waitForTimeout(80);
  const walBefore10 = await readOpfsWalLength(page, "sample-1m");

  // Drag fill handle on `A2` down 10 rows (`A2:A12`, rows 1..11).
  await page.evaluate(() => window.__reticula?.select(1, 0));
  await expect(page.getByTestId("name-box")).toHaveText("A2");
  const { box, metrics } = await gridOriginAndMetrics(page);
  const handleA2X = box.x + metrics.headerWidth + metrics.colWidth;
  const handleA2Y = box.y + metrics.headerHeight + metrics.rowHeight * 2;
  await page.mouse.move(handleA2X, handleA2Y);
  await page.mouse.down();
  await page.mouse.move(handleA2X, box.y + metrics.headerHeight + metrics.rowHeight * 11.5, {
    steps: 4,
  });
  await page.mouse.up();

  await expect(page.getByTestId("name-box")).toHaveText("A2:A12");
  await settled(page);
  await page.waitForTimeout(80);
  const walAfter10 = await readOpfsWalLength(page, "sample-1m");

  // Now select `A12` (`row = 11, col = 0`) and double-click its fill handle to auto-fill down to row 1,000,000!
  await page.evaluate(() => window.__reticula?.select(11, 0));
  await expect(page.getByTestId("name-box")).toHaveText("A12");
  const handleA12X = box.x + metrics.headerWidth + metrics.colWidth;
  const handleA12Y = box.y + metrics.headerHeight + metrics.rowHeight * 12;
  await page.mouse.dblclick(handleA12X, handleA12Y);

  // Formula count jumps by ~999,988 (from ~100,012 to ~1,100,000)!
  await expect(page.getByTestId("document")).toContainText("1,100,000 formulas", {
    timeout: 15_000,
  });
  await page.waitForTimeout(80);
  const walAfter1M = await readOpfsWalLength(page, "sample-1m");

  if (walBefore10 !== null && walAfter10 !== null && walAfter1M !== null) {
    const delta10 = walAfter10 - walBefore10;
    const delta1M = walAfter1M - walAfter10;
    expect(delta10).toBeGreaterThan(20);
    expect(delta10).toBeLessThan(260);
    expect(delta1M).toBeGreaterThan(20);
    expect(delta1M).toBeLessThan(260);
    expect(Math.abs(delta1M - delta10)).toBeLessThan(32);
  }

  // Scroll to the very bottom (`row = 999,999`) and verify `A1000000` holds `=F1000000*2`.
  await page.evaluate(() => {
    window.__reticula?.scrollTo({ top: Number.MAX_SAFE_INTEGER, left: 0 });
    window.__reticula?.select(999_999, 0);
  });
  await settled(page);
  await expect(page.getByTestId("name-box")).toHaveText("A1000000");
  await expect(page.getByTestId("formula-input")).toHaveText("=F1000000*2");
});

test("OPFS SeedOrigin + WAL reload, background CoW snapshot slot flip, and torn-tail truncation preserve edits", async ({
  page,
}) => {
  await openWorkbook(page);
  const backend = await page.evaluate(() => window.__reticula?.docStatus().storageBackend);
  test.skip(
    backend === "memory",
    "Browser lacks persistent OPFS in this environment (e.g. WinCairo WebKit)",
  );

  await editCellViaBar(page, 1, 0, "A2", "PersistedOrder-777", "PersistedOrder-777");
  expect(await page.evaluate(() => window.__reticula?.textAt(1, 0))).toBe("PersistedOrder-777");

  // Reload page: `SeedOrigin` stub + `wal.log` replay restores the edit cleanly.
  await page.reload();
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await settled(page);
  expect(await page.evaluate(() => window.__reticula?.textAt(1, 0))).toBe("PersistedOrder-777");

  // Compact snapshot via background CoW stream -> flips `activeSlot` from `"a"` to `"b"`.
  const initialSlot = await page.evaluate(() => window.__reticula?.docStatus().activeSlot);
  expect(initialSlot).toBe("a");
  const saved = await page.evaluate(() => window.__reticula?.compactSnapshot());
  expect(saved?.slot).toBe("b");

  // Edit another cell on top of snapshot slot `"b"`, flush, then append a torn crash tail to `wal.log`!
  await editCellViaBar(page, 2, 0, "A3", "AfterCompact-888");
  await page.waitForTimeout(80);
  await page.evaluate(() => window.__reticula?.flushAndClose());

  const tornBytesAppended = await page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    const v1 = await root.getDirectoryHandle("reticula-v1");
    const local = await v1.getDirectoryHandle("local");
    const dir = await local.getDirectoryHandle("sample-1m");
    const fh = await dir.getFileHandle("wal.log");
    const existing = new Uint8Array(await (await fh.getFile()).arrayBuffer());
    // Simulate a torn crash record at EOF (incomplete 8-byte header + partial payload with bad CRC).
    const tornTail = new Uint8Array([24, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef, 1, 2, 3]);
    const combined = new Uint8Array(existing.byteLength + tornTail.byteLength);
    combined.set(existing, 0);
    combined.set(tornTail, existing.byteLength);
    if (typeof fh.createWritable === "function") {
      const w = await fh.createWritable();
      await w.write(combined);
      await w.close();
      return combined.byteLength;
    }
    // Safari / WebKit fallback: write via transient inline Worker `SyncAccessHandle`.
    await new Promise<void>((resolve, reject) => {
      const blob = new Blob(
        [
          `onmessage = async (e) => {
            try {
              const root = await navigator.storage.getDirectory();
              const dir = await (await (await root.getDirectoryHandle("reticula-v1")).getDirectoryHandle("local")).getDirectoryHandle("sample-1m");
              const fh = await dir.getFileHandle("wal.log");
              const s = await fh.createSyncAccessHandle();
              s.truncate(0);
              s.write(e.data, { at: 0 });
              s.flush();
              s.close();
              postMessage("ok");
            } catch (err) { postMessage({ err: String(err) }); }
          };`,
        ],
        { type: "text/javascript" },
      );
      const w = new Worker(URL.createObjectURL(blob));
      w.onmessage = (ev) => {
        w.terminate();
        ev.data === "ok" ? resolve() : reject(new Error(ev.data?.err));
      };
      w.postMessage(combined);
    });
    return combined.byteLength;
  });

  // Reopen: committed edits survive and the torn final record is silently truncated!
  await page.reload();
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await settled(page);
  expect(await page.evaluate(() => window.__reticula?.textAt(1, 0))).toBe("PersistedOrder-777");
  expect(await page.evaluate(() => window.__reticula?.textAt(2, 0))).toBe("AfterCompact-888");

  const truncatedLen = await readOpfsWalLength(page, "sample-1m");
  expect(truncatedLen).not.toBeNull();
  expect(truncatedLen as number).toBeLessThan(tornBytesAppended);
});

test("two tabs opening the same local workbook enforce single-instance lock, yield cleanly, and survive hung-worker takeover", async ({
  context,
}) => {
  const tab1 = await context.newPage();
  await openWorkbook(tab1, "doc=multi-tab-doc");
  const backend = await tab1.evaluate(() => window.__reticula?.docStatus().storageBackend);
  test.skip(
    backend === "memory",
    "Browser lacks persistent OPFS in this environment (e.g. WinCairo WebKit)",
  );

  await editCellViaBar(tab1, 0, 0, "A1", "TabOneValue", "TabOneValue");
  expect(await tab1.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("TabOneValue");

  // Open the same local doc in Tab 2 -> blocked by Web Lock!
  const tab2 = await context.newPage();
  await tab2.goto("/app?hooks&doc=multi-tab-doc");
  await expect(tab2.getByTestId("engine-status")).toHaveText("blocked");
  await expect(tab2.getByTestId("lock-overlay")).toContainText("open in another tab");

  // Step 1–2 normal takeover: Tab 2 clicks "Open here (Take over)" -> Tab 1 yields and Tab 2 reads `TabOneValue`.
  await tab2.getByTestId("take-over-btn").click();
  await expect(tab2.getByTestId("engine-status")).toHaveText("ready");
  await settled(tab2);
  await expect(tab1.getByTestId("engine-status")).toHaveText("yielded");
  expect(await tab2.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("TabOneValue");

  // Edit in Tab 2, then spin Tab 2's worker for 2.5 s while Tab 1 takes back over -> 500 ms main-thread watchdog terminates Tab 2's hung worker!
  await editCellViaBar(tab2, 1, 0, "A2", "TabTwoBeforeHang");
  await tab2.waitForTimeout(80);
  await tab2.evaluate(() => window.__reticula?.spinWorker(2500));

  await tab1.bringToFront();
  await tab1.getByTestId("take-over-btn").click();
  await expect(tab1.getByTestId("engine-status")).toHaveText("ready", { timeout: 10_000 });
  await settled(tab1);
  await expect(tab2.getByTestId("engine-status")).toHaveText("yielded");
  expect(await tab1.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("TabOneValue");
  expect(await tab1.evaluate(() => window.__reticula?.textAt(1, 0))).toBe("TabTwoBeforeHang");

  await tab1.close();
  await tab2.close();
});

test("mid-file WAL corruption enters Safe Mode (raw .zip export, salvage recovery, paused recalc) and CSV/.rtcl import-export work", async ({
  page,
}) => {
  await openWorkbook(page, "doc=safemode-doc");
  const backend = await page.evaluate(() => window.__reticula?.docStatus().storageBackend);
  test.skip(
    backend === "memory",
    "Browser lacks persistent OPFS in this environment (e.g. WinCairo WebKit)",
  );

  await editCellViaBar(page, 0, 0, "A1", "SafeTxn1");
  await editCellViaBar(page, 1, 0, "A2", "SafeTxn2");
  await editCellViaBar(page, 2, 0, "A3", "SafeTxn3");
  await page.waitForTimeout(80);
  await page.evaluate(() => window.__reticula?.flushAndClose());

  // Corrupt a byte in the middle of `wal.log` (after record 1, before record 3) so trailing non-zero bytes trigger `MidFileCorruption`!
  await page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    const dir = await (
      await (await root.getDirectoryHandle("reticula-v1")).getDirectoryHandle("local")
    ).getDirectoryHandle("safemode-doc");
    const fh = await dir.getFileHandle("wal.log");
    const bytes = new Uint8Array(await (await fh.getFile()).arrayBuffer());
    // Flip a payload byte roughly halfway through the file (inside record 2 while record 3 follows).
    const midPos = Math.floor(bytes.byteLength * 0.62);
    bytes[midPos] = (bytes[midPos] ?? 0) ^ 0x55;
    if (typeof fh.createWritable === "function") {
      const w = await fh.createWritable();
      await w.write(bytes);
      await w.close();
      return;
    }
    await new Promise<void>((resolve, reject) => {
      const blob = new Blob(
        [
          `onmessage = async (e) => {
            try {
              const root = await navigator.storage.getDirectory();
              const dir = await (await (await root.getDirectoryHandle("reticula-v1")).getDirectoryHandle("local")).getDirectoryHandle("safemode-doc");
              const fh = await dir.getFileHandle("wal.log");
              const s = await fh.createSyncAccessHandle();
              s.truncate(0);
              s.write(e.data, { at: 0 });
              s.flush();
              s.close();
              postMessage("ok");
            } catch (err) { postMessage({ err: String(err) }); }
          };`,
        ],
        { type: "text/javascript" },
      );
      const w = new Worker(URL.createObjectURL(blob));
      w.onmessage = (ev) => {
        w.terminate();
        ev.data === "ok" ? resolve() : reject(new Error(ev.data?.err));
      };
      w.postMessage(bytes);
    });
  });

  // Reopening MUST refuse silent truncation and enter Safe Mode!
  await page.reload();
  await expect(page.getByTestId("engine-status")).toHaveText("safeMode");
  await expect(page.getByTestId("safe-mode-modal")).toBeVisible();

  // Escape Hatch 3: Export raw OPFS files as a `.zip` archive without running WASM decode.
  const rawZipPromise = page.waitForEvent("download");
  await page.getByTestId("safe-export-raw").click();
  const rawZip = await rawZipPromise;
  expect(rawZip.suggestedFilename()).toBe("safemode-doc-raw-backup.zip");
  const zipPath = await rawZip.path();
  expect(zipPath).toBeTruthy();
  if (zipPath) {
    const zipBuf = await readFile(zipPath);
    expect(zipBuf.subarray(0, 4).toString("binary")).toBe("PK\x03\x04");
  }

  // Escape Hatch 2: Salvage verified prefix (`SafeTxn1`) and save a clean snapshot.
  await expect(page.getByTestId("safe-skip-count")).toHaveValue("0");
  await page.getByTestId("safe-skip-tail-btn").click();
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await settled(page);
  await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("SafeTxn1");

  // Escape Hatch 1 (`?safe=1` -> Open Paused -> Resume Recalculation):
  await page.goto("/app?hooks&doc=safemode-doc&safe=1");
  await expect(page.getByTestId("engine-status")).toHaveText("safeMode");
  await page.getByTestId("safe-open-frozen").click();
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await expect(page.getByTestId("recalc-frozen-banner")).toBeVisible();
  await page.getByTestId("resume-recalc-btn").click();
  await expect(page.getByTestId("recalc-frozen-banner")).toHaveCount(0);

  // `.rtcl` and `.csv` export + RFC 4180 CSV import (keeping `= Promo =` as plain text, not formula error).
  await page.getByTestId("workbooks-btn").click();
  await expect(page.getByTestId("workbooks-drawer")).toBeVisible();

  const csvExportPromise = page.waitForEvent("download");
  await page.getByTestId("drawer-export-csv").click();
  const csvDownload = await csvExportPromise;
  expect(csvDownload.suggestedFilename()).toMatch(/\.csv$/);

  const rtclExportPromise = page.waitForEvent("download");
  await page.getByTestId("drawer-export-rtcl").click();
  const rtclDownload = await rtclExportPromise;
  expect(rtclDownload.suggestedFilename()).toMatch(/\.rtcl$/);

  const sampleCsv = Buffer.from(
    'Item,Note,Amount\r\n"= Special Promo =","Quoted, ""Line""",125.5\r\n',
    "utf8",
  );
  await page.getByTestId("import-file-input").setInputFiles({
    name: "imported-items.csv",
    mimeType: "text/csv",
    buffer: sampleCsv,
  });

  await expect(page.getByTestId("workbooks-drawer")).toHaveCount(0);
  await expect(page.getByTestId("document")).toContainText("imported-items");
  await settled(page);
  expect(await page.evaluate(() => window.__reticula?.textAt(1, 0))).toBe("= Special Promo =");
  expect(await page.evaluate(() => window.__reticula?.textAt(1, 1))).toBe('Quoted, "Line"');
  expect(await page.evaluate(() => window.__reticula?.textAt(1, 2))).toBe("125.5");
});
